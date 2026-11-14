//! Messages to and from the IR.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use agent_core::strng;
use agent_http::Body;
use bytes::Bytes;
use serde::de::DeserializeOwned;

use serde_json::Value;

use super::{
	Citation, Content, Effort, Message, Part, ReplyPart, Request, Response, ResponseFormat, Role,
	Source, StopReason, StreamEvent, Thinking, Tool, ToolChoice, Usage,
};
use crate::conversion::completions::parse_data_url;
use crate::conversion::messages::StreamingToolCalls;
use crate::conversion::{GeminiThoughtSignature, ProviderUsage, ResponseWithProviderUsage};
use crate::parse::sse::SseJsonEvent;
use crate::types::ResponseType;
use crate::types::messages::AccountingUsage;
use crate::types::messages::typed as messages;
use crate::{AIError, StreamingUsageGuard, json, logged_response_parsing, types};

pub fn parse_request(req: &types::messages::Request) -> Result<Request, AIError> {
	let req = json::convert::<_, messages::Request>(req)
		.map_err(|err| AIError::RequestParsing(crate::InputFormat::Messages, err))?;
	let instructions = req.system.map(|system| Message {
		role: Role::System,
		content: match system {
			messages::SystemPrompt::Text(text) => Content::Text(text),
			messages::SystemPrompt::Blocks(blocks) => Content::Parts(
				blocks
					.into_iter()
					.map(
						|messages::SystemContentBlock::Text {
						   text,
						   cache_control,
						 }| Part::Text {
							text,
							cache: cache_control.is_some(),
						},
					)
					.collect(),
			),
		},
	});
	let msgs = req.messages.into_iter().filter_map(message).collect();

	let mut parallel_tool_calls = None;
	let tool_choice = req.tool_choice.map(|choice| match choice {
		messages::ToolChoice::Auto {
			disable_parallel_tool_use,
		} => {
			parallel_tool_calls = disable_parallel_tool_use.map(|d| !d);
			ToolChoice::Auto
		},
		messages::ToolChoice::Any {
			disable_parallel_tool_use,
		} => {
			parallel_tool_calls = disable_parallel_tool_use.map(|d| !d);
			ToolChoice::Required
		},
		messages::ToolChoice::Tool {
			name,
			disable_parallel_tool_use,
		} => {
			parallel_tool_calls = disable_parallel_tool_use.map(|d| !d);
			ToolChoice::Tool(name)
		},
		messages::ToolChoice::None {} => ToolChoice::None,
	});
	let tools = req.tools.map(|tools| {
		tools
			.into_iter()
			.filter_map(|tool| match tool {
				messages::Tool::Custom(tool) => Some(Tool {
					name: tool.name,
					description: tool.description,
					parameters: Some(tool.input_schema),
					strict: tool.strict,
					cache: tool.cache_control.is_some(),
				}),
				messages::Tool::Server(tool) => {
					tracing::warn!("dropping unsupported server tool {:?}", tool.tool_type);
					None
				},
			})
			.collect()
	});
	let output_config = req.output_config.unwrap_or_default();
	Ok(Request {
		model: Some(req.model),
		instructions,
		messages: msgs,
		tools,
		tool_choice,
		parallel_tool_calls,
		max_tokens: Some(req.max_tokens),
		temperature: req.temperature,
		top_p: req.top_p,
		stop: req.stop_sequences,
		stream: req.stream,
		metadata: req
			.metadata
			.map(|metadata| metadata.fields.into_iter().collect())
			.unwrap_or_default(),
		thinking: req.thinking.map(|thinking| match thinking {
			messages::ThinkingInput::Enabled { budget_tokens, .. } => Thinking::Enabled { budget_tokens },
			messages::ThinkingInput::Adaptive { .. } => Thinking::Adaptive,
			messages::ThinkingInput::Disabled {} => Thinking::Disabled,
		}),
		effort: output_config
			.effort
			.map(super::completions::messages_effort),
		response_format: output_config
			.format
			.map(
				|messages::OutputFormat::JsonSchema { schema }| ResponseFormat::JsonSchema {
					name: None,
					description: None,
					schema,
					strict: None,
				},
			),
	})
}

/// Content no renderer carries is dropped here.
fn message(msg: messages::Message) -> Option<Message> {
	// Gemini signatures ride in thinking blocks; they belong to the tool call they name.
	let mut gemini_signatures: HashMap<_, _> = msg
		.content
		.iter()
		.filter_map(|block| match block {
			messages::ContentBlock::Thinking { signature, .. } => {
				GeminiThoughtSignature::decode(signature)
			},
			_ => None,
		})
		.map(|s| (s.call_id, s.signature))
		.collect();
	let role = match msg.role {
		messages::Role::User => Role::User,
		messages::Role::Assistant => Role::Assistant,
		messages::Role::System => Role::System,
	};
	let parts: Vec<_> = msg
		.content
		.into_iter()
		.filter_map(|block| match block {
			messages::ContentBlock::Text(block) => Some(Part::Text {
				text: block.text,
				cache: block.cache_control.is_some(),
			}),
			messages::ContentBlock::Image(block) if role != Role::System => Some(Part::Image {
				source: image_source(&block.source),
				cache: block.cache_control.is_some(),
			}),
			messages::ContentBlock::Document(block) if role != Role::System => Some(Part::Document {
				source: document_source(&block.source),
				title: block.title,
				cache: block.cache_control.is_some(),
			}),
			messages::ContentBlock::ToolUse {
				id,
				name,
				input,
				cache_control,
			} if role != Role::System => Some(Part::ToolCall {
				cache: cache_control.is_some(),
				thought_signature: gemini_signatures.remove(&id),
				arguments: serde_json::to_string(&input).unwrap_or_default(),
				id,
				name,
			}),
			messages::ContentBlock::ToolResult {
				tool_use_id,
				content,
				cache_control,
				is_error,
			} if role != Role::System => Some(Part::ToolResult {
				id: tool_use_id,
				content: tool_content(content),
				is_error,
				cache: cache_control.is_some(),
			}),
			messages::ContentBlock::Thinking {
				thinking,
				signature,
			} if role != Role::System && GeminiThoughtSignature::decode(&signature).is_none() => {
				Some(Part::Reasoning {
					text: thinking,
					signature,
				})
			},
			_ => None,
		})
		.collect();
	if role == Role::System && parts.is_empty() {
		return None;
	}
	Some(Message {
		role,
		content: Content::Parts(parts),
	})
}

fn tool_content(content: messages::ToolResultContent) -> Content {
	match content {
		messages::ToolResultContent::Text(text) => Content::Text(text),
		messages::ToolResultContent::Array(parts) => Content::Parts(
			parts
				.into_iter()
				.filter_map(|part| match part {
					messages::ToolResultContentPart::Text {
						text,
						cache_control,
						..
					}
					| messages::ToolResultContentPart::ToolReference {
						tool_name: text,
						cache_control,
					} => Some(Part::Text {
						text,
						cache: cache_control.is_some(),
					}),
					messages::ToolResultContentPart::Image {
						source,
						cache_control,
					} => Some(Part::Image {
						source: image_source(&source),
						cache: cache_control.is_some(),
					}),
					messages::ToolResultContentPart::Document {
						source,
						cache_control,
						title,
						..
					} => Some(Part::Document {
						source: document_source(&source),
						title,
						cache: cache_control.is_some(),
					}),
					messages::ToolResultContentPart::SearchResult { cache_control, .. } => {
						Some(Part::Opaque {
							cache: cache_control.is_some(),
						})
					},
					messages::ToolResultContentPart::Unknown => None,
				})
				.collect(),
		),
	}
}

/// A Messages image source. Base64 data becomes a `data:` URL.
fn image_source(source: &Value) -> Source {
	let field = |name: &str| source.get(name).and_then(Value::as_str).map(str::to_string);
	let Some(kind) = field("type") else {
		return Source::Invalid("messages image source missing type");
	};
	match kind.as_str() {
		"base64" => match (field("media_type"), field("data")) {
			(None, _) => Source::Invalid("messages base64 image source missing media_type"),
			(_, None) => Source::Invalid("messages base64 image source missing data"),
			(Some(media_type), Some(data)) => Source::Url(format!("data:{media_type};base64,{data}")),
		},
		"url" => field("url").map_or(
			Source::Invalid("messages url image source missing url"),
			Source::Url,
		),
		"file" => field("file_id").map_or(
			Source::Invalid("messages file image source missing file_id"),
			Source::FileId,
		),
		_ => Source::Invalid("messages image source cannot be represented by responses"),
	}
}

/// A Messages document source. Base64 data becomes a `data:` URL.
fn document_source(source: &Value) -> Source {
	let field = |name: &str| source.get(name).and_then(Value::as_str).map(str::to_string);
	let Some(kind) = field("type") else {
		return Source::Invalid("messages document source missing type");
	};
	match kind.as_str() {
		"base64" => match (field("media_type"), field("data")) {
			(None, _) => Source::Invalid("messages base64 document source missing media_type"),
			(_, None) => Source::Invalid("messages base64 document source missing data"),
			(Some(media_type), Some(data)) => Source::Url(format!("data:{media_type};base64,{data}")),
		},
		"url" => field("url").map_or(
			Source::Invalid("messages url document source missing url"),
			Source::Url,
		),
		"file" => field("file_id").map_or(
			Source::Invalid("messages file document source missing file_id"),
			Source::FileId,
		),
		"text" => field("data").map_or(
			Source::Invalid("messages text document source missing data"),
			Source::Text,
		),
		_ => Source::Invalid("messages document source cannot be represented by responses"),
	}
}

pub fn render_request(
	req: Request,
	catalog: crate::model_catalog::Catalog<'_>,
) -> Result<messages::Request, AIError> {
	let cache_control =
		|cache: bool| cache.then_some(messages::CacheControlEphemeral::Ephemeral { ttl: None });
	// Messages rejects empty text blocks.
	let text = |text: String, cache: bool| {
		(!text.trim().is_empty()).then(|| {
			messages::ContentBlock::Text(messages::ContentTextBlock {
				text,
				citations: None,
				cache_control: cache_control(cache),
			})
		})
	};

	let mut system = Vec::new();
	let mut msgs = Vec::new();
	for msg in req.instructions.into_iter().chain(req.messages) {
		if msg.role.is_system() {
			let parts = match msg.content {
				Content::Text(text) => vec![(text, false)],
				Content::Parts(parts) => parts
					.into_iter()
					.filter_map(|part| match part {
						Part::Text { text, cache } => Some((text, cache)),
						_ => None,
					})
					.collect(),
			};
			system.extend(
				parts
					.into_iter()
					.filter(|(text, _)| !text.trim().is_empty())
					.map(|(text, cache)| messages::SystemContentBlock::Text {
						text,
						cache_control: cache_control(cache),
					}),
			);
			continue;
		}
		let parts = match msg.content {
			Content::Text(t) => vec![Part::Text {
				text: t,
				cache: false,
			}],
			Content::Parts(parts) => parts,
		};
		let mut content = Vec::new();
		for part in parts {
			match part {
				Part::Text { text: t, cache } => content.extend(text(t, cache)),
				Part::Image {
					source: Source::Url(url),
					cache,
				} => {
					let source = match parse_data_url(&url) {
						Some((media_type, data)) => {
							serde_json::json!({"type": "base64", "media_type": media_type, "data": data})
						},
						None => serde_json::json!({"type": "url", "url": url}),
					};
					content.push(messages::ContentBlock::Image(messages::ContentImageBlock {
						source,
						cache_control: cache_control(cache),
					}));
				},
				Part::Document {
					source,
					title,
					cache,
				} => {
					let inline = match &source {
						Source::Url(url) => parse_data_url(url),
						_ => None,
					};
					let Some((media_type, data)) = inline else {
						return Err(AIError::UnsupportedConversion(match source {
							Source::FileId(id) => strng::new(format!(
								"anthropic path cannot resolve OpenAI file_id ({id}); send file.file_data as an inline data: URI"
							)),
							_ => strng::literal!(
								"file content part requires file.file_data as an inline base64 data: URI"
							),
						}));
					};
					content.push(messages::ContentBlock::Document(
						messages::ContentDocumentBlock {
							source: serde_json::json!({"type": "base64", "media_type": media_type, "data": data}),
							cache_control: cache_control(cache),
							citations: None,
							context: None,
							title,
						},
					));
				},
				Part::ToolCall {
					id,
					name,
					arguments,
					cache,
					..
				} => content.push(messages::ContentBlock::ToolUse {
					id,
					name,
					input: crate::conversion::tool_arguments_to_input(&arguments),
					cache_control: cache_control(cache),
				}),
				Part::ToolResult {
					id,
					content: result,
					is_error,
					cache,
				} => content.push(messages::ContentBlock::ToolResult {
					tool_use_id: id,
					content: match result {
						Content::Text(text) => messages::ToolResultContent::Text(text),
						Content::Parts(parts) => messages::ToolResultContent::Array(
							parts
								.into_iter()
								.filter_map(|part| match part {
									Part::Text { text, cache } => Some(messages::ToolResultContentPart::Text {
										text,
										citations: None,
										cache_control: cache_control(cache),
									}),
									_ => None,
								})
								.collect(),
						),
					},
					cache_control: cache_control(cache),
					is_error,
				}),
				// A replayed thinking block must carry the signature it was issued with.
				Part::Reasoning {
					text: thinking,
					signature,
				} if !signature.is_empty() => content.push(messages::ContentBlock::Thinking {
					thinking,
					signature,
				}),
				_ => {},
			}
		}
		if !content.is_empty() {
			msgs.push(messages::Message {
				role: match msg.role {
					Role::Assistant => messages::Role::Assistant,
					_ => messages::Role::User,
				},
				content,
			});
		}
	}
	let system = if system.is_empty() {
		None
	} else if system
		.iter()
		.any(|messages::SystemContentBlock::Text { cache_control, .. }| cache_control.is_some())
	{
		Some(messages::SystemPrompt::Blocks(system))
	} else {
		Some(messages::SystemPrompt::Text(
			system
				.into_iter()
				.map(|messages::SystemContentBlock::Text { text, .. }| text)
				.collect::<Vec<_>>()
				.join("\n"),
		))
	};

	let tools = req.tools.map(|tools| {
		tools
			.into_iter()
			.map(|tool| {
				messages::Tool::Custom(messages::CustomTool {
					name: tool.name,
					description: tool.description,
					input_schema: tool.parameters.unwrap_or_default(),
					strict: None,
					cache_control: cache_control(tool.cache),
				})
			})
			.collect::<Vec<_>>()
	});
	let has_tools = tools.as_ref().is_some_and(|tools| !tools.is_empty());
	let disable_parallel_tool_use = req.parallel_tool_calls.map(|p| !p);
	let tool_choice = match req.tool_choice {
		Some(ToolChoice::Tool(name)) => Some(messages::ToolChoice::Tool {
			name,
			disable_parallel_tool_use,
		}),
		Some(ToolChoice::Auto) => Some(messages::ToolChoice::Auto {
			disable_parallel_tool_use,
		}),
		Some(ToolChoice::Required) => Some(messages::ToolChoice::Any {
			disable_parallel_tool_use,
		}),
		Some(ToolChoice::None) => Some(messages::ToolChoice::None {}),
		None if disable_parallel_tool_use.is_some() && has_tools => Some(messages::ToolChoice::Auto {
			disable_parallel_tool_use,
		}),
		Some(ToolChoice::Other) | None => None,
	};

	let model = req.model.unwrap_or_default();
	let capabilities = crate::model_catalog::anthropic_thinking_capabilities(&model, catalog);
	let max_tokens = req.max_tokens.unwrap_or(4096);
	// Messages has no `minimal`.
	let effort = req.effort.map(|effort| match effort {
		Effort::Minimal | Effort::Low => messages::ThinkingEffort::Low,
		Effort::Medium => messages::ThinkingEffort::Medium,
		Effort::High => messages::ThinkingEffort::High,
		Effort::Xhigh => messages::ThinkingEffort::Xhigh,
		Effort::Max => messages::ThinkingEffort::Max,
	});
	let explicit_budget = match req.thinking {
		Some(Thinking::Enabled { budget_tokens }) => Some(budget_tokens),
		_ => None,
	};
	let enabled = |budget_tokens: Option<u64>| {
		budget_tokens
			.and_then(|budget| {
				crate::conversion::messages::cap_thinking_budget_to_max_tokens(budget, max_tokens)
			})
			.map(|budget_tokens| messages::ThinkingInput::Enabled {
				budget_tokens,
				display: None,
			})
	};
	let (thinking, effort) = match (explicit_budget, effort) {
		(Some(_), _) if capabilities.legacy => (enabled(explicit_budget), None),
		(Some(_), effort) | (None, effort @ Some(_)) if capabilities.adaptive => (
			Some(messages::ThinkingInput::Adaptive { display: None }),
			Some(effort.unwrap_or(messages::ThinkingEffort::High)),
		),
		(budget, effort) => (
			enabled(budget.or(effort.map(crate::types::thinking_budget_for_anthropic_effort))),
			None,
		),
	};

	let format = req
		.response_format
		.map(|format| messages::OutputFormat::JsonSchema {
			schema: match format {
				ResponseFormat::JsonSchema { schema, .. } => schema,
				ResponseFormat::JsonObject => {
					serde_json::json!({"type": "object", "additionalProperties": true})
				},
			},
		});
	let output_config =
		(format.is_some() || effort.is_some()).then_some(messages::OutputConfig { effort, format });

	Ok(messages::Request {
		messages: msgs,
		system,
		model,
		max_tokens,
		stop_sequences: req.stop,
		stream: req.stream,
		temperature: req.temperature,
		top_p: req.top_p,
		top_k: None,
		tools,
		tool_choice,
		metadata: (!req.metadata.is_empty()).then(|| messages::Metadata {
			fields: req.metadata.into_iter().collect(),
		}),
		thinking,
		output_config,
		cache_control: None,
	})
}

pub fn parse_response(bytes: &Bytes) -> Result<Response, AIError> {
	let resp = serde_json::from_slice::<messages::MessagesResponse>(bytes)
		.map_err(logged_response_parsing(bytes))?;
	let accounting = resp.usage.accounting();
	let provider_usage = ProviderUsage {
		input_tokens: accounting.input_tokens,
		total_tokens: accounting
			.input_tokens
			.saturating_add(accounting.output_tokens),
		cache_creation_1h_input_tokens: accounting.cache_creation_1h_input_tokens,
		usage_lines: crate::types::messages::usage_lines(&resp.usage.iterations),
		..Default::default()
	};
	let parts = resp
		.content
		.into_iter()
		.filter_map(|block| match block {
			messages::ContentBlock::Text(block) => Some(ReplyPart::Text {
				text: block.text,
				citations: Vec::new(),
			}),
			messages::ContentBlock::ToolUse {
				id, name, input, ..
			}
			| messages::ContentBlock::ServerToolUse {
				id, name, input, ..
			} => Some(ReplyPart::ToolCall {
				arguments: serde_json::to_string(&input).ok()?,
				id,
				name,
				thought_signature: None,
			}),
			messages::ContentBlock::Thinking {
				thinking,
				signature,
			} => Some(ReplyPart::Reasoning {
				text: thinking,
				signature,
			}),
			_ => None,
		})
		.collect();
	Ok(Response {
		id: resp.id,
		model: resp.model,
		parts,
		stop_reason: resp.stop_reason,
		stop_sequence: resp.stop_sequence,
		usage: Some(usage(&accounting)),
		service_tier: resp.usage.service_tier,
		provider_usage: Some(provider_usage),
	})
}

pub fn render_response(resp: Response) -> Result<Box<dyn ResponseType>, AIError> {
	let mut content = Vec::new();
	for part in resp.parts {
		match part {
			ReplyPart::Text { text, citations } => {
				// Messages keeps a citation's source, not an excerpt or a replay index, so those are
				// left empty.
				let citations: Vec<_> = citations
					.into_iter()
					.map(|Citation { url, title }| {
						serde_json::json!({
							"type": "web_search_result_location",
							"url": url,
							"title": title,
							"cited_text": "",
							"encrypted_index": "",
						})
					})
					.collect();
				content.push(messages::ContentBlock::Text(messages::ContentTextBlock {
					text,
					citations: (!citations.is_empty()).then_some(Value::Array(citations)),
					cache_control: None,
				}));
			},
			ReplyPart::Refusal(text) => content.push(messages::ContentBlock::text(text)),
			ReplyPart::Reasoning { text, signature } => content.push(messages::ContentBlock::Thinking {
				thinking: text,
				signature,
			}),
			ReplyPart::ToolCall {
				id,
				name,
				arguments,
				thought_signature,
			} => {
				// A Gemini signature rides in a thinking block ahead of its tool call.
				if let Some(signature) = thought_signature {
					content.push(messages::ContentBlock::Thinking {
						thinking: String::new(),
						signature: GeminiThoughtSignature::encode(&id, &signature),
					});
				}
				content.push(messages::ContentBlock::ToolUse {
					input: crate::conversion::tool_arguments_to_input(&arguments),
					id,
					name,
					cache_control: None,
				});
			},
		}
	}
	let usage = resp.usage.unwrap_or_default();
	let cache_write = usage.cache_write_tokens.unwrap_or_default();
	let cache_read = usage.cache_read_tokens.unwrap_or_default();
	let response = messages::MessagesResponse {
		content,
		stop_reason: resp.stop_reason,
		stop_sequence: resp.stop_sequence,
		usage: messages::Usage {
			// Messages counts cached input apart from the rest.
			input_tokens: usage
				.input_tokens
				.saturating_sub(cache_write)
				.saturating_sub(cache_read) as usize,
			output_tokens: usage.output_tokens as usize,
			cache_creation_input_tokens: usage.cache_write_tokens.map(|t| t as usize),
			cache_read_input_tokens: usage.cache_read_tokens.map(|t| t as usize),
			service_tier: resp.service_tier,
			..Default::default()
		},
		input_audio_tokens: usage.input_audio_tokens.map(|t| t as usize),
		output_audio_tokens: usage.output_audio_tokens.map(|t| t as usize),
		..messages::MessagesResponse::start(resp.id, resp.model)
	};
	Ok(Box::new(ResponseWithProviderUsage {
		response,
		provider_usage: resp.provider_usage,
	}))
}

fn usage(accounting: &AccountingUsage) -> Usage {
	Usage {
		// Messages counts cached input apart from the rest.
		input_tokens: accounting
			.input_tokens
			.saturating_add(accounting.cache_read_input_tokens.unwrap_or_default())
			.saturating_add(accounting.cache_creation_input_tokens.unwrap_or_default()),
		output_tokens: accounting.output_tokens,
		total_tokens: None,
		reasoning_tokens: None,
		cache_read_tokens: accounting.cache_read_input_tokens,
		cache_write_tokens: accounting.cache_creation_input_tokens,
		input_audio_tokens: None,
		output_audio_tokens: None,
	}
}

/// Reads a Messages provider stream into events, recording its usage, timing and content to `log`.
pub fn parse_stream(
	log: StreamingUsageGuard,
	log_content: crate::LogContentFields,
) -> impl FnMut(SseJsonEvent<messages::MessagesStreamEvent>) -> Vec<StreamEvent> + Send + 'static {
	let mut accounting = AccountingUsage::default();
	let mut last_token_at: Option<Instant> = None;
	let mut tool_blocks = HashSet::new();
	let mut completion = log_content.completion.then(String::new);
	let mut tool_calls = StreamingToolCalls::new(log_content.tool_calls);
	move |event| {
		// Nothing can be done with an event that doesn't parse.
		let SseJsonEvent::Data(Ok(event)) = event else {
			return Vec::new();
		};
		match event {
			messages::MessagesStreamEvent::MessageStart { message } => {
				accounting = message.usage.accounting();
				log.update(|r| {
					accounting.apply(&mut r.response);
					r.response.service_tier = crate::types::messages::served_tier(
						message.usage.service_tier.as_deref(),
						message.usage.speed.as_deref(),
					);
					r.response.provider_model = Some(strng::new(&message.model))
				});
				vec![StreamEvent::Start {
					id: message.id,
					model: message.model,
					service_tier: message.usage.service_tier,
				}]
			},
			messages::MessagesStreamEvent::ContentBlockStart {
				index,
				content_block:
					messages::ContentBlock::ToolUse {
						id, name, input, ..
					}
					| messages::ContentBlock::ServerToolUse {
						id, name, input, ..
					},
			} => {
				// `input` is always `{}` here: the arguments arrive as deltas.
				tool_calls.start(index, id.as_str(), name.as_str(), &input);
				tool_blocks.insert(index);
				vec![StreamEvent::ToolCall {
					index,
					id: Some(id),
					name: Some(name),
					arguments: String::new(),
				}]
			},
			messages::MessagesStreamEvent::ContentBlockStart { .. } => Vec::new(),
			messages::MessagesStreamEvent::ContentBlockDelta { delta, index } => {
				let now = Instant::now();
				match last_token_at.replace(now) {
					None => log.update(|r| r.response.first_token = Some(now)),
					Some(prev) => log.update(|r| {
						r.response
							.inter_chunk_latencies
							.record(now.duration_since(prev))
					}),
				}
				match delta {
					messages::ContentBlockDelta::TextDelta { text } => {
						if let Some(completion) = completion.as_mut() {
							completion.push_str(&text);
						}
						vec![StreamEvent::Text { index, text }]
					},
					messages::ContentBlockDelta::ThinkingDelta { thinking } => {
						vec![StreamEvent::Reasoning(thinking)]
					},
					messages::ContentBlockDelta::InputJsonDelta { partial_json } => {
						tool_calls.append_arguments(index, &partial_json);
						vec![StreamEvent::ToolCall {
							index,
							id: None,
							name: None,
							arguments: partial_json,
						}]
					},
					messages::ContentBlockDelta::SignatureDelta { signature } => {
						vec![StreamEvent::Signature(signature)]
					},
					messages::ContentBlockDelta::CitationsDelta { .. } => Vec::new(),
				}
			},
			messages::MessagesStreamEvent::ContentBlockStop { index } => {
				if tool_blocks.remove(&index) {
					vec![StreamEvent::ToolCallEnd { index }]
				} else {
					Vec::new()
				}
			},
			messages::MessagesStreamEvent::MessageDelta {
				usage: delta_usage,
				delta,
			} => {
				accounting = delta_usage.accumulate(accounting);
				let finish_reason = delta
					.stop_reason
					.as_ref()
					.and_then(crate::types::serialize_str);
				log.update(|r| {
					accounting.apply(&mut r.response);
					if !delta_usage.iterations.is_empty() {
						r.response.usage_lines = crate::types::messages::usage_lines(&delta_usage.iterations);
					}
					if let Some(completion) = completion.take() {
						r.response.completion = Some(vec![completion]);
					}
					r.response.output_messages = tool_calls.take_output_messages(finish_reason.clone());
				});
				vec![StreamEvent::End {
					stop_reason: delta.stop_reason,
					stop_sequence: delta.stop_sequence,
					usage: Some(usage(&accounting)),
				}]
			},
			messages::MessagesStreamEvent::MessageStop => {
				finish_log(&log, &mut completion, &mut tool_calls);
				Vec::new()
			},
			messages::MessagesStreamEvent::Ping => Vec::new(),
			messages::MessagesStreamEvent::Error { error } => {
				finish_log(&log, &mut completion, &mut tool_calls);
				vec![StreamEvent::Error {
					kind: error.r#type,
					message: error.message,
				}]
			},
		}
	}
}

/// Logs the content of a reply that ended without a `message_delta` to log it.
fn finish_log(
	log: &StreamingUsageGuard,
	completion: &mut Option<String>,
	tool_calls: &mut StreamingToolCalls,
) {
	log.update(|r| {
		if let Some(completion) = completion.take() {
			r.response.completion = Some(vec![completion]);
		}
		if r.response.output_messages.is_none() {
			r.response.output_messages = tool_calls.take_output_messages(None);
		}
	});
}

/// Writes events as a Messages client stream. Messages has one open block at a time, except that
/// tool calls stay open across one another, and a block's signature is sent as it closes.
pub fn render_stream<I: DeserializeOwned + 'static>(
	body: Body,
	buffer_limit: usize,
	mut parse: impl FnMut(SseJsonEvent<I>) -> Vec<StreamEvent> + Send + 'static,
) -> Body {
	let mut blocks = Blocks::default();
	crate::parse::sse::json_transform_multi::<I, messages::MessagesStreamEvent, _>(
		body,
		buffer_limit,
		move |event| {
			let mut out = Vec::new();
			for event in parse(event) {
				blocks.write(event, &mut out);
			}
			out
		},
	)
}

type Out = Vec<(&'static str, messages::MessagesStreamEvent)>;

fn push(out: &mut Out, event: messages::MessagesStreamEvent) {
	out.push((event.event_name(), event));
}

#[derive(Default)]
struct ToolBlock {
	id: Option<String>,
	name: Option<String>,
	/// Arguments that arrived before the name, sent once the block opens.
	pending: String,
	block: Option<usize>,
}

#[derive(Default)]
struct Blocks {
	next: usize,
	/// Open text blocks by text part. A part that continues after its block closed gets a new one.
	texts: HashMap<usize, usize>,
	thinking: Option<usize>,
	signature: Option<String>,
	tools: HashMap<usize, ToolBlock>,
	open_tools: HashSet<usize>,
}

impl Blocks {
	fn write(&mut self, event: StreamEvent, out: &mut Out) {
		match event {
			StreamEvent::Start { id, model, .. } => push(
				out,
				messages::MessagesStreamEvent::MessageStart {
					message: messages::MessagesResponse::start(id, model),
				},
			),
			StreamEvent::Reasoning(thinking) => {
				let index = self.open_thinking(out);
				push(
					out,
					messages::MessagesStreamEvent::ContentBlockDelta {
						index,
						delta: messages::ContentBlockDelta::ThinkingDelta { thinking },
					},
				);
			},
			// Reasoning that is withheld arrives as a signature alone, before any block, and
			// opens one of its own to carry it.
			StreamEvent::Signature(signature) => {
				if self.thinking.is_some() || self.next == 0 {
					self.open_thinking(out);
					self.signature = Some(signature);
				}
			},
			StreamEvent::TextStart { index } => {
				self.open_text(index, out);
			},
			StreamEvent::Text { index, text } => {
				let index = self.open_text(index, out);
				push(
					out,
					messages::MessagesStreamEvent::ContentBlockDelta {
						index,
						delta: messages::ContentBlockDelta::TextDelta { text },
					},
				);
			},
			StreamEvent::TextEnd { index } => {
				if let Some(index) = self.texts.remove(&index) {
					push(
						out,
						messages::MessagesStreamEvent::ContentBlockStop { index },
					);
				}
			},
			// A call's block opens once its name is known.
			StreamEvent::ToolCall {
				index,
				id,
				name,
				arguments,
			} => {
				let tool = self.tools.entry(index).or_default();
				if id.is_some() {
					tool.id = id;
				}
				if name.is_some() {
					tool.name = name;
				}
				tool.pending.push_str(&arguments);
				if tool.name.is_none() {
					return;
				}
				let block = self.open_tool(index, out);
				let tool = self.tools.get_mut(&index).expect("opened above");
				let json = std::mem::take(&mut tool.pending);
				if !json.is_empty() {
					push(
						out,
						messages::MessagesStreamEvent::ContentBlockDelta {
							index: block,
							delta: messages::ContentBlockDelta::InputJsonDelta { partial_json: json },
						},
					);
				}
			},
			StreamEvent::ToolCallEnd { index } => {
				if self.open_tools.remove(&index)
					&& let Some(block) = self.tools.get(&index).and_then(|tool| tool.block)
				{
					push(
						out,
						messages::MessagesStreamEvent::ContentBlockStop { index: block },
					);
				}
			},
			StreamEvent::End {
				stop_reason,
				stop_sequence,
				usage,
			} => {
				self.close_thinking(out);
				self.close_texts(out);
				self.close_tools(out);
				let usage = usage.unwrap_or_default();
				let cache_write = usage.cache_write_tokens.unwrap_or_default();
				let cache_read = usage.cache_read_tokens.unwrap_or_default();
				push(
					out,
					messages::MessagesStreamEvent::MessageDelta {
						delta: messages::MessageDelta {
							stop_reason: Some(stop_reason.unwrap_or(StopReason::EndTurn)),
							stop_sequence,
						},
						usage: messages::MessageDeltaUsage {
							// Messages counts cached input apart from the rest.
							input_tokens: Some(
								usage
									.input_tokens
									.saturating_sub(cache_write)
									.saturating_sub(cache_read) as usize,
							),
							output_tokens: Some(usage.output_tokens as usize),
							cache_creation_input_tokens: usage.cache_write_tokens.map(|t| t as usize),
							cache_read_input_tokens: usage.cache_read_tokens.map(|t| t as usize),
							..Default::default()
						},
					},
				);
				push(out, messages::MessagesStreamEvent::MessageStop);
			},
			StreamEvent::Error { kind, message } => push(
				out,
				messages::MessagesStreamEvent::Error {
					error: messages::MessagesError {
						r#type: kind,
						message,
					},
				},
			),
		}
	}

	fn open_block(&mut self, block: messages::ContentBlock, out: &mut Out) -> usize {
		let index = self.next;
		self.next += 1;
		push(
			out,
			messages::MessagesStreamEvent::ContentBlockStart {
				index,
				content_block: block,
			},
		);
		index
	}

	fn open_thinking(&mut self, out: &mut Out) -> usize {
		if let Some(index) = self.thinking {
			return index;
		}
		self.close_texts(out);
		self.close_tools(out);
		let index = self.open_block(
			messages::ContentBlock::Thinking {
				thinking: String::new(),
				signature: String::new(),
			},
			out,
		);
		*self.thinking.insert(index)
	}

	fn open_text(&mut self, part: usize, out: &mut Out) -> usize {
		if let Some(index) = self.texts.get(&part) {
			return *index;
		}
		self.close_tools(out);
		self.close_thinking(out);
		let index = self.open_block(messages::ContentBlock::text(String::new()), out);
		self.texts.insert(part, index);
		index
	}

	/// Opens a tool call's block, or reopens it under the same index after another block closed
	/// it.
	fn open_tool(&mut self, index: usize, out: &mut Out) -> usize {
		self.close_texts(out);
		self.close_thinking(out);
		let next = &mut self.next;
		let tool = self.tools.get_mut(&index).expect("a call opens once known");
		let block = *tool.block.get_or_insert_with(|| {
			*next += 1;
			*next - 1
		});
		if self.open_tools.insert(index) {
			push(
				out,
				messages::MessagesStreamEvent::ContentBlockStart {
					index: block,
					content_block: messages::ContentBlock::ToolUse {
						id: tool
							.id
							.clone()
							.unwrap_or_else(|| format!("tool_call_{index}")),
						name: tool.name.clone().unwrap_or_default(),
						input: Value::Object(serde_json::Map::new()),
						cache_control: None,
					},
				},
			);
		}
		block
	}

	fn close_texts(&mut self, out: &mut Out) {
		let mut open: Vec<_> = self.texts.drain().map(|(_, index)| index).collect();
		open.sort();
		for index in open {
			push(
				out,
				messages::MessagesStreamEvent::ContentBlockStop { index },
			);
		}
	}

	fn close_thinking(&mut self, out: &mut Out) {
		if let Some(index) = self.thinking.take() {
			if let Some(signature) = self.signature.take() {
				push(
					out,
					messages::MessagesStreamEvent::ContentBlockDelta {
						index,
						delta: messages::ContentBlockDelta::SignatureDelta { signature },
					},
				);
			}
			push(
				out,
				messages::MessagesStreamEvent::ContentBlockStop { index },
			);
		}
	}

	fn close_tools(&mut self, out: &mut Out) {
		let mut open: Vec<_> = self
			.open_tools
			.drain()
			.filter_map(|index| self.tools.get(&index)?.block)
			.collect();
		open.sort();
		for index in open {
			push(
				out,
				messages::MessagesStreamEvent::ContentBlockStop { index },
			);
		}
	}
}
