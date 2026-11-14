//! Chat Completions to and from the IR.

use std::collections::HashMap;
use std::time::Instant;

use agent_core::strng;
use agent_http::Body;
use bytes::Bytes;
use serde::de::DeserializeOwned;

use super::{
	Content, Effort, Message, Part, Request, Response, ResponseFormat, Role, StopReason, StreamEvent,
	Thinking, Tool, ToolChoice, Usage,
};
use crate::conversion::{ProviderUsage, ResponseWithProviderUsage};
use crate::parse::sse::SseJsonEvent;
use crate::types::ResponseType;
use crate::types::completions::typed as completions;
use crate::types::messages::typed::MessagesError;
use crate::{AIError, StreamingUsageGuard, json, logged_response_parsing, types};

pub fn parse_request(req: &types::completions::Request) -> Result<Request, AIError> {
	let req = json::convert::<_, completions::Request>(req)
		.map_err(|err| AIError::RequestParsing(crate::InputFormat::Completions, err))?;
	let max_tokens = req.max_tokens();
	let stop = req.stop_sequence();
	let messages = req.messages.into_iter().map(message).collect();
	let tools = req.tools.map(|tools| {
		tools
			.into_iter()
			.filter_map(|tool| match tool {
				completions::Tool::Function(tool) => Some(Tool {
					name: tool.function.name,
					description: tool.function.description,
					parameters: tool.function.parameters,
					strict: tool.function.strict,
					cache: false,
				}),
				_ => None,
			})
			.collect()
	});
	let tool_choice = req.tool_choice.map(|choice| match choice {
		completions::ToolChoiceOption::Function(choice) => ToolChoice::Tool(choice.function.name),
		completions::ToolChoiceOption::Mode(completions::ToolChoiceOptions::Auto) => ToolChoice::Auto,
		completions::ToolChoiceOption::Mode(completions::ToolChoiceOptions::Required) => {
			ToolChoice::Required
		},
		completions::ToolChoiceOption::Mode(completions::ToolChoiceOptions::None) => ToolChoice::None,
		_ => ToolChoice::Other,
	});
	let response_format = match req.response_format {
		Some(completions::ResponseFormat::JsonSchema { json_schema }) => {
			Some(ResponseFormat::JsonSchema(json_schema.schema))
		},
		Some(completions::ResponseFormat::JsonObject) => Some(ResponseFormat::JsonObject),
		Some(completions::ResponseFormat::Text) | None => None,
	};
	Ok(Request {
		model: req.model.unwrap_or_default(),
		messages,
		tools,
		tool_choice,
		parallel_tool_calls: req.parallel_tool_calls,
		max_tokens,
		temperature: req.temperature,
		top_p: req.top_p,
		stop,
		stream: req.stream.unwrap_or(false),
		user: req.user,
		thinking: req
			.vendor_extensions
			.thinking_budget_tokens
			.map(|budget_tokens| Thinking::Enabled { budget_tokens }),
		effort: req
			.reasoning_effort
			.as_ref()
			.and_then(crate::types::anthropic_effort_for_reasoning_effort),
		response_format,
	})
}

fn message(msg: completions::RequestMessage) -> Message {
	let text = |part: completions::RequestMessageContentPartText| Part::Text {
		text: part.text,
		cache: part.prompt_cache_breakpoint.is_some(),
	};
	let (role, content) = match msg {
		completions::RequestMessage::System(msg) => (
			Role::System,
			match msg.content {
				completions::RequestSystemMessageContent::Text(t) => Content::Text(t),
				completions::RequestSystemMessageContent::Array(parts) => Content::Parts(
					parts
						.into_iter()
						.map(|completions::RequestSystemMessageContentPart::Text(t)| text(t))
						.collect(),
				),
			},
		),
		completions::RequestMessage::Developer(msg) => (
			Role::System,
			match msg.content {
				completions::RequestDeveloperMessageContent::Text(t) => Content::Text(t),
				completions::RequestDeveloperMessageContent::Array(parts) => Content::Parts(
					parts
						.into_iter()
						.map(|completions::RequestDeveloperMessageContentPart::Text(t)| text(t))
						.collect(),
				),
			},
		),
		completions::RequestMessage::User(msg) => (
			Role::User,
			match msg.content {
				completions::RequestUserMessageContent::Text(t) => Content::Text(t),
				completions::RequestUserMessageContent::Array(parts) => Content::Parts(
					parts
						.into_iter()
						.filter_map(|part| match part {
							completions::RequestUserMessageContentPart::Text(t) => Some(text(t)),
							completions::RequestUserMessageContentPart::ImageUrl(image) => Some(Part::Image {
								url: image.image_url.url,
								cache: image.prompt_cache_breakpoint.is_some(),
							}),
							completions::RequestUserMessageContentPart::File(file) => Some(Part::File {
								data: file.file.file_data,
								id: file.file.file_id,
								name: file.file.filename,
								cache: file.prompt_cache_breakpoint.is_some(),
							}),
							completions::RequestUserMessageContentPart::InputAudio(_) => None,
						})
						.collect(),
				),
			},
		),
		completions::RequestMessage::Assistant(msg) => {
			let mut parts = Vec::new();
			if msg.reasoning_content.is_some() || msg.reasoning_signature.is_some() {
				parts.push(Part::Reasoning {
					text: msg.reasoning_content.unwrap_or_default(),
					signature: msg.reasoning_signature.unwrap_or_default(),
				});
			}
			match msg.content {
				Some(completions::RequestAssistantMessageContent::Text(t)) => parts.push(Part::Text {
					text: t,
					cache: false,
				}),
				Some(completions::RequestAssistantMessageContent::Array(content)) => {
					parts.extend(content.into_iter().map(|part| match part {
						completions::RequestAssistantMessageContentPart::Text(t) => text(t),
						completions::RequestAssistantMessageContentPart::Refusal(r) => Part::Text {
							text: r.refusal,
							cache: false,
						},
					}))
				},
				None => {},
			}
			if let Some(refusal) = msg.refusal {
				parts.push(Part::Text {
					text: refusal,
					cache: false,
				});
			}
			parts.extend(msg.tool_calls.into_iter().flatten().map(|call| match call {
				completions::MessageToolCalls::Function(call) => Part::ToolCall {
					thought_signature: call.thought_signature().map(str::to_string),
					cache: false,
					id: call.id,
					name: call.function.name,
					arguments: call.function.arguments,
				},
				completions::MessageToolCalls::Custom(call) => Part::ToolCall {
					id: call.id,
					name: call.custom_tool.name,
					arguments: call.custom_tool.input,
					thought_signature: None,
					cache: false,
				},
			}));
			(Role::Assistant, Content::Parts(parts))
		},
		completions::RequestMessage::Tool(msg) => (
			Role::User,
			Content::Parts(vec![Part::ToolResult {
				id: msg.tool_call_id,
				content: match msg.content {
					completions::RequestToolMessageContent::Text(t) => Content::Text(t),
					completions::RequestToolMessageContent::Array(parts) => Content::Parts(
						parts
							.into_iter()
							.map(|completions::RequestToolMessageContentPart::Text(t)| text(t))
							.collect(),
					),
				},
				is_error: None,
				cache: false,
			}]),
		),
		completions::RequestMessage::Function(msg) => (
			Role::User,
			Content::Parts(
				msg
					.content
					.into_iter()
					.map(|text| Part::Text { text, cache: false })
					.collect(),
			),
		),
	};
	Message { role, content }
}

#[allow(deprecated)]
pub fn render_request(req: Request) -> completions::Request {
	let model = req.model;
	// Explicit prompt-cache breakpoints are accepted only by GPT 5.6 and newer models.
	let caches = crate::conversion::supports_prompt_cache_breakpoint(&model);
	let text_part = |text: String, cache: bool| completions::RequestMessageContentPartText {
		text,
		prompt_cache_breakpoint: breakpoint(cache && caches),
	};

	let output_effort = req.effort.or(match req.thinking {
		Some(Thinking::Enabled { budget_tokens }) => Some(
			crate::types::anthropic_effort_for_thinking_budget(budget_tokens),
		),
		_ => None,
	});
	let reasoning_requested = match req.thinking {
		Some(Thinking::Disabled) => false,
		Some(Thinking::Adaptive) => true,
		_ => output_effort.is_some(),
	};
	let reasoning_effort = (reasoning_requested
		&& crate::conversion::supports_reasoning_effort(&model))
	.then(|| match output_effort {
		Some(Effort::Low) => completions::ReasoningEffort::Low,
		Some(Effort::Medium) => completions::ReasoningEffort::Medium,
		Some(Effort::High) => completions::ReasoningEffort::High,
		Some(Effort::Xhigh) => completions::ReasoningEffort::Xhigh,
		Some(Effort::Max) => completions::ReasoningEffort::Max,
		// Anthropic adaptive thinking defaults to high effort when omitted.
		None => completions::ReasoningEffort::High,
	});

	let mut msgs = Vec::new();
	for msg in req.messages {
		match (msg.role, msg.content) {
			(Role::System, content) => {
				let content = match content {
					Content::Text(t) => completions::RequestSystemMessageContent::Text(t),
					Content::Parts(parts) => completions::RequestSystemMessageContent::Array(
						parts
							.into_iter()
							.filter_map(|part| match part {
								Part::Text { text, cache } => Some(
									completions::RequestSystemMessageContentPart::Text(text_part(text, cache)),
								),
								_ => None,
							})
							.collect(),
					),
				};
				msgs.push(completions::RequestMessage::System(
					completions::RequestSystemMessage {
						content,
						name: None,
					},
				));
			},
			(Role::User, Content::Text(t)) => {
				msgs.push(completions::RequestMessage::User(
					completions::RequestUserMessage {
						content: completions::RequestUserMessageContent::Text(t),
						name: None,
					},
				));
			},
			(Role::User, Content::Parts(parts)) => {
				// Tool results become their own messages, ahead of the turn's other content.
				let mut user = Vec::new();
				for part in parts {
					match part {
						Part::Text { text, cache } => user.push(
							completions::RequestUserMessageContentPart::Text(text_part(text, cache)),
						),
						Part::Image { url, .. } => {
							user.push(completions::RequestUserMessageContentPart::ImageUrl(
								completions::RequestMessageContentPartImage {
									image_url: completions::ImageUrl { url, detail: None },
									// OpenAI rejects cache breakpoints on image parts even though the
									// Chat Completions schema exposes this field.
									prompt_cache_breakpoint: None,
								},
							))
						},
						Part::ToolResult {
							id, content, cache, ..
						} => msgs.push(completions::RequestMessage::Tool(
							completions::RequestToolMessage {
								content: tool_content(content, cache, caches),
								tool_call_id: id,
							},
						)),
						_ => {},
					}
				}
				if !user.is_empty() {
					msgs.push(completions::RequestMessage::User(
						completions::RequestUserMessage {
							content: completions::RequestUserMessageContent::Array(user),
							name: None,
						},
					));
				}
			},
			(Role::Assistant, content) => {
				let parts = match content {
					Content::Text(text) => vec![Part::Text { text, cache: false }],
					Content::Parts(parts) => parts,
				};
				let mut text = Vec::new();
				let mut tool_calls = Vec::new();
				let mut reasoning_content: Option<String> = None;
				let mut signatures = Vec::new();
				for part in parts {
					match part {
						Part::Text { text: t, cache } => text.push(
							completions::RequestAssistantMessageContentPart::Text(text_part(t, cache)),
						),
						Part::ToolCall {
							id,
							name,
							arguments,
							thought_signature,
							..
						} => {
							let mut call = completions::MessageToolCall {
								id,
								function: completions::FunctionCall { name, arguments },
								extra_content: None,
							};
							if let Some(signature) = thought_signature {
								call.set_thought_signature(signature);
							}
							tool_calls.push(completions::MessageToolCalls::Function(call));
						},
						// Chat Completions carries one reasoning text per turn, so the parts are joined.
						Part::Reasoning {
							text: thinking,
							signature,
						} => {
							match reasoning_content.as_mut() {
								Some(text) => {
									text.push_str("\n\n");
									text.push_str(&thinking);
								},
								None => reasoning_content = Some(thinking),
							}
							signatures.push(signature);
						},
						_ => {},
					}
				}
				if text.is_empty() && tool_calls.is_empty() && reasoning_content.is_none() {
					continue;
				}
				msgs.push(completions::RequestMessage::Assistant(
					completions::RequestAssistantMessage {
						content: (!text.is_empty())
							.then_some(completions::RequestAssistantMessageContent::Array(text)),
						tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
						reasoning_content,
						// A signature attests to one block, so it only survives a turn with exactly one.
						reasoning_signature: match signatures.as_slice() {
							[signature] if !signature.is_empty() => Some(signature.clone()),
							_ => None,
						},
						..Default::default()
					},
				));
			},
		}
	}

	let tools: Vec<_> = req
		.tools
		.into_iter()
		.flatten()
		.map(|tool| {
			completions::Tool::Function(completions::FunctionTool {
				function: completions::FunctionObject {
					name: tool.name,
					description: tool.description,
					parameters: tool.parameters,
					strict: tool.strict,
				},
			})
		})
		.collect();

	// OpenAI rejects reasoning with tools on newer GPT-5 models over Chat Completions.
	let reasoning_with_tools = !model.starts_with("gpt-5")
		|| model == "gpt-5"
		|| model.starts_with("gpt-5-")
		|| model.starts_with("gpt-5.1")
		|| model.starts_with("gpt-5.2");
	let reasoning_effort = if !tools.is_empty() && !reasoning_with_tools {
		Some(completions::ReasoningEffort::None)
	} else {
		reasoning_effort
	};

	let tool_choice = req.tool_choice.and_then(|choice| match choice {
		ToolChoice::Auto => Some(completions::ToolChoiceOption::Mode(
			completions::ToolChoiceOptions::Auto,
		)),
		ToolChoice::Required => Some(completions::ToolChoiceOption::Mode(
			completions::ToolChoiceOptions::Required,
		)),
		ToolChoice::None => Some(completions::ToolChoiceOption::Mode(
			completions::ToolChoiceOptions::None,
		)),
		ToolChoice::Tool(name) => Some(completions::ToolChoiceOption::Function(
			completions::NamedToolChoice {
				function: completions::FunctionName { name },
			},
		)),
		ToolChoice::Other => None,
	});

	completions::Request {
		model: Some(model),
		moderation: None,
		messages: msgs,
		stream: Some(req.stream),
		temperature: req.temperature,
		top_p: req.top_p,
		max_completion_tokens: Some(req.max_tokens as u32),
		max_tokens: None,
		stop: (!req.stop.is_empty()).then_some(completions::Stop::StringArray(req.stop)),
		tools: (!tools.is_empty()).then_some(tools),
		tool_choice,
		user: req.user,
		vendor_extensions: completions::RequestVendorExtensions::default(),
		stream_options: req.stream.then_some(completions::StreamOptions {
			include_usage: Some(true),
			include_obfuscation: None,
		}),
		frequency_penalty: None,
		presence_penalty: None,
		seed: None,
		store: None,
		reasoning_effort,
		metadata: None,
		logit_bias: None,
		logprobs: None,
		top_logprobs: None,
		n: None,
		modalities: None,
		prediction: None,
		audio: None,
		response_format: req.response_format.map(|format| {
			let schema = match format {
				ResponseFormat::JsonSchema(schema) => schema,
				ResponseFormat::JsonObject => {
					serde_json::json!({"type": "object", "additionalProperties": true})
				},
			};
			completions::ResponseFormat::JsonSchema {
				json_schema: completions::ResponseFormatJsonSchema {
					description: None,
					name: "structured_output".to_string(),
					schema,
					strict: None,
				},
			}
		}),
		function_call: None,
		functions: None,
		service_tier: None,
		parallel_tool_calls: req.parallel_tool_calls,
		web_search_options: None,
	}
}

fn breakpoint(cache: bool) -> Option<completions::PromptCacheBreakpointParam> {
	cache.then_some(completions::PromptCacheBreakpointParam {
		mode: completions::PromptCacheBreakpointParamMode::Explicit,
	})
}

/// A tool result's content. Parts this format can't carry still pass on their cache mark, to the
/// last text part or to an empty one.
fn tool_content(
	content: Content,
	cache: bool,
	caches: bool,
) -> completions::RequestToolMessageContent {
	let part = |text: String, cache: bool| {
		completions::RequestToolMessageContentPart::Text(completions::RequestMessageContentPartText {
			text,
			prompt_cache_breakpoint: breakpoint(cache && caches),
		})
	};
	let parts = match content {
		Content::Text(text) if cache && caches => vec![(text, true)],
		Content::Text(text) => return completions::RequestToolMessageContent::Text(text),
		Content::Parts(parts) => {
			let mut trailing = cache;
			let mut out = Vec::new();
			for p in parts {
				match p {
					Part::Text { text, cache } => out.push((text, cache)),
					Part::Opaque { cache } => trailing |= cache,
					_ => {},
				}
			}
			if trailing && caches {
				match out.last_mut() {
					Some((_, cache)) => *cache = true,
					None => out.push((String::new(), true)),
				}
			}
			out
		},
	};
	completions::RequestToolMessageContent::Array(
		parts
			.into_iter()
			.map(|(text, cache)| part(text, cache))
			.collect(),
	)
}

pub fn parse_response(bytes: &Bytes, model: &str) -> Result<Response, AIError> {
	let resp = serde_json::from_slice::<completions::Response>(bytes)
		.map_err(logged_response_parsing(bytes))?;
	let provider_usage = resp.usage.as_ref().map(|u| ProviderUsage {
		input_tokens: u.prompt_tokens as u64,
		total_tokens: u.total_tokens as u64,
		reasoning_tokens: u
			.completion_tokens_details
			.as_ref()
			.and_then(|d| d.reasoning_tokens),
		..Default::default()
	});
	let choice = resp
		.choices
		.into_iter()
		.next()
		.ok_or_else(|| AIError::InvalidResponse(strng::literal!("chat response missing choices")))?;

	let mut parts = Vec::new();
	let thinking = choice.message.reasoning().unwrap_or_default().to_string();
	let signature = choice.message.reasoning_signature.unwrap_or_default();
	if !thinking.is_empty() || !signature.is_empty() {
		parts.push(Part::Reasoning {
			text: thinking,
			signature,
		});
	}
	if let Some(text) = choice.message.content {
		parts.push(Part::Text { text, cache: false });
	}
	parts.extend(
		choice
			.message
			.tool_calls
			.into_iter()
			.flatten()
			.filter_map(|call| match call {
				completions::MessageToolCalls::Function(call) => Some(Part::ToolCall {
					thought_signature: call.thought_signature().map(str::to_string),
					cache: false,
					id: call.id,
					name: call.function.name,
					arguments: call.function.arguments,
				}),
				completions::MessageToolCalls::Custom(_) => None,
			}),
	);

	let (mut stop_reason, may_have_stop_sequence) = match choice.finish_reason {
		Some(completions::FinishReason::Stop) => (StopReason::EndTurn, true),
		Some(completions::FinishReason::Length) => (StopReason::MaxTokens, false),
		Some(completions::FinishReason::ToolCalls | completions::FinishReason::FunctionCall) => {
			(StopReason::ToolUse, false)
		},
		Some(completions::FinishReason::ContentFilter) => (StopReason::Refusal, false),
		None => (StopReason::EndTurn, false),
	};
	let stop_sequence = may_have_stop_sequence
		.then(|| crate::conversion::completions::from_messages::choice_stop_sequence(&choice.rest))
		.flatten();
	if stop_sequence.is_some() {
		stop_reason = StopReason::StopSequence;
	}

	let usage = resp.usage.as_ref().map(usage).unwrap_or_default();

	Ok(Response {
		id: resp.id,
		model: if resp.model.is_empty() {
			model.to_string()
		} else {
			resp.model
		},
		parts,
		stop_reason: Some(stop_reason),
		stop_sequence,
		usage,
		service_tier: resp.service_tier,
		provider_usage,
	})
}

pub fn render_response(resp: Response) -> Result<Box<dyn ResponseType>, AIError> {
	let mut content: Option<String> = None;
	let mut tool_calls = Vec::new();
	let mut reasoning_content: Option<String> = None;
	let mut signatures = Vec::new();
	for part in resp.parts {
		match part {
			// Chat Completions has one text and one reasoning field, so several parts are joined.
			Part::Text { text, .. } => content.get_or_insert_default().push_str(&text),
			Part::Reasoning { text, signature } => {
				match reasoning_content.as_mut() {
					Some(content) => {
						content.push_str("\n\n");
						content.push_str(&text);
					},
					None => reasoning_content = Some(text),
				}
				signatures.push(signature);
			},
			Part::ToolCall {
				id,
				name,
				arguments,
				thought_signature,
				..
			} => {
				let mut call = completions::MessageToolCall {
					id,
					function: completions::FunctionCall { name, arguments },
					extra_content: None,
				};
				if let Some(signature) = thought_signature {
					call.set_thought_signature(signature);
				}
				tool_calls.push(completions::MessageToolCalls::Function(call));
			},
			_ => {},
		}
	}
	let message = completions::ResponseMessage {
		content,
		tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
		reasoning_content,
		reasoning_signature: match signatures.as_slice() {
			[signature] if !signature.is_empty() => Some(signature.clone()),
			_ => None,
		},
		..completions::ResponseMessage::assistant()
	};
	let finish_reason = resp.stop_reason.map(finish_reason);
	let usage = resp.usage;
	let openai = completions::Response {
		id: resp.id,
		object: "chat.completion".to_string(),
		// The IR has no creation time, so it is now.
		created: chrono::Utc::now().timestamp() as u32,
		model: resp.model,
		choices: vec![completions::ChatChoice {
			rest: Default::default(),
			index: 0,
			message,
			finish_reason,
			logprobs: None,
		}],
		usage: Some(completions::Usage {
			prompt_tokens: usage.input_tokens as u32,
			completion_tokens: usage.output_tokens as u32,
			total_tokens: usage.input_tokens.saturating_add(usage.output_tokens) as u32,
			cache_read_input_tokens: usage.cache_read_tokens,
			prompt_tokens_details: completions::UsagePromptDetails::from_cache_tokens(
				usage.cache_read_tokens,
				usage.cache_write_tokens,
			),
			cache_creation_input_tokens: usage.cache_write_tokens,
			..Default::default()
		}),
		service_tier: resp.service_tier,
		system_fingerprint: None,
	};
	let response =
		json::convert::<_, types::completions::Response>(&openai).map_err(AIError::ResponseParsing)?;
	Ok(Box::new(ResponseWithProviderUsage {
		response,
		provider_usage: resp.provider_usage,
	}))
}

fn usage(u: &completions::Usage) -> Usage {
	let prompt = u.prompt_tokens_details.as_ref();
	Usage {
		input_tokens: u.prompt_tokens as u64,
		output_tokens: u.output_tokens() as u64,
		cache_read_tokens: prompt
			.and_then(|d| d.cached_tokens)
			.or(u.cache_read_input_tokens),
		cache_write_tokens: prompt
			.and_then(|d| d.cache_write_tokens)
			.or(u.cache_creation_input_tokens),
		input_audio_tokens: prompt.and_then(|d| d.audio_tokens),
		output_audio_tokens: u
			.completion_tokens_details
			.as_ref()
			.and_then(|d| d.audio_tokens),
	}
}

/// Reads a Completions provider stream into events, recording its usage, timing and tool calls to
/// `log`. Completions sends the finish reason and the usage in separate chunks, so the reply
/// ends once it has both, or at `[DONE]`.
pub fn parse_stream(
	log: StreamingUsageGuard,
	log_content: crate::LogContentFields,
) -> impl FnMut(SseJsonEvent<completions::StreamResponse>) -> Vec<StreamEvent> + Send + 'static {
	struct ToolCall {
		id: Option<String>,
		name: Option<String>,
		started: bool,
		/// Arguments that arrived before the name, sent once it is known.
		pending: String,
		/// Every argument, for the log.
		arguments: String,
	}
	#[derive(Default)]
	struct State {
		started: bool,
		ended: bool,
		last_token_at: Option<Instant>,
		tool_calls: HashMap<u32, ToolCall>,
		stop_reason: Option<StopReason>,
		stop_sequence: Option<String>,
		usage: Option<completions::Usage>,
	}
	impl State {
		fn token(&mut self, log: &StreamingUsageGuard) {
			let now = Instant::now();
			match self.last_token_at.replace(now) {
				None => log.update(|r| r.response.first_token = Some(now)),
				Some(prev) => log.update(|r| {
					r.response
						.inter_chunk_latencies
						.record(now.duration_since(prev))
				}),
			}
		}

		/// Ends the reply once its finish reason and usage are known, or regardless when `force`d.
		fn end(
			&mut self,
			out: &mut Vec<StreamEvent>,
			log: &StreamingUsageGuard,
			force: bool,
			log_tool_calls: bool,
		) {
			if self.ended || (!force && (self.stop_reason.is_none() || self.usage.is_none())) {
				return;
			}
			self.ended = true;
			let stop_reason = self.stop_reason.take().unwrap_or(StopReason::EndTurn);
			let provider_usage = self.usage.take();
			let usage = provider_usage.as_ref().map(usage);
			if let (Some(provider_usage), Some(usage)) = (&provider_usage, &usage) {
				log.update(|r| {
					r.response.input_tokens = Some(provider_usage.prompt_tokens as u64);
					r.response.output_tokens = Some(provider_usage.output_tokens() as u64);
					r.response.total_tokens = Some(provider_usage.total_tokens as u64);
					r.response.reasoning_tokens = provider_usage
						.completion_tokens_details
						.as_ref()
						.and_then(|d| d.reasoning_tokens);
					r.response.cache_creation_input_tokens = usage.cache_write_tokens;
					r.response.cached_input_tokens = usage.cache_read_tokens;
				});
			}
			if log_tool_calls
				&& let Some(parts) = crate::conversion::completions::finalize_streaming_tool_calls(
					self
						.tool_calls
						.drain()
						.map(|(index, call)| (index, call.id, call.name, call.arguments)),
				) {
				let mut parts = Some(parts);
				let mut finish_reason = crate::types::serialize_str(&stop_reason);
				log.update(|r| {
					crate::conversion::completions::build_output_messages(
						&mut r.response,
						parts.take(),
						finish_reason.take(),
					);
				});
			}
			out.push(StreamEvent::End {
				stop_reason: Some(stop_reason),
				stop_sequence: self.stop_sequence.take(),
				usage,
			});
		}
	}

	let mut state = State::default();
	move |event| {
		let mut out = Vec::new();
		let chunk = match event {
			SseJsonEvent::Done => {
				state.end(&mut out, &log, true, log_content.tool_calls);
				return out;
			},
			SseJsonEvent::Data(Ok(chunk)) => chunk,
			SseJsonEvent::Data(Err(e)) => {
				tracing::debug!("failed to parse a Completions stream chunk: {e}");
				return out;
			},
			SseJsonEvent::Eof | SseJsonEvent::Error => return out,
		};
		if !state.started {
			state.started = true;
			log.update(|r| r.response.provider_model = Some(strng::new(&chunk.model)));
			out.push(StreamEvent::Start {
				id: chunk.id.clone(),
				model: chunk.model.clone(),
				service_tier: None,
			});
		}
		if let Some(usage) = chunk.usage {
			state.usage = Some(usage);
		}
		if let Some(choice) = chunk.choices.first() {
			let delta = &choice.delta;
			if let Some(thinking) = delta.reasoning() {
				state.token(&log);
				out.push(StreamEvent::Reasoning(thinking.to_string()));
			}
			if let Some(signature) = delta
				.reasoning_signature
				.as_deref()
				.filter(|s| !s.is_empty())
			{
				out.push(StreamEvent::Signature(signature.to_string()));
			}
			if let Some(text) = delta.content.as_deref().filter(|s| !s.is_empty()) {
				state.token(&log);
				out.push(StreamEvent::Text(text.to_string()));
			}
			for tool in delta.tool_calls.iter().flatten() {
				let call = state.tool_calls.entry(tool.index).or_insert(ToolCall {
					id: None,
					name: None,
					started: false,
					pending: String::new(),
					arguments: String::new(),
				});
				if let Some(id) = &tool.id {
					call.id = Some(id.clone());
				}
				if let Some(function) = &tool.function {
					if let Some(name) = &function.name {
						call.name = Some(name.clone());
					}
					if let Some(arguments) = &function.arguments {
						call.pending.push_str(arguments);
						if log_content.tool_calls {
							call.arguments.push_str(arguments);
						}
					}
				}
				// A call starts once its name is known.
				let Some(name) = call.name.clone() else {
					continue;
				};
				let index = tool.index as usize;
				if !call.started {
					call.started = true;
					out.push(StreamEvent::ToolCallStart {
						index,
						id: call
							.id
							.clone()
							.unwrap_or_else(|| format!("tool_call_{index}")),
						name,
					});
				}
				let json = std::mem::take(&mut call.pending);
				if !json.is_empty() {
					state.token(&log);
				}
				out.push(StreamEvent::ToolCallArguments { index, json });
			}
			if let Some(finish_reason) = &choice.finish_reason {
				let mut stop_reason = match finish_reason {
					completions::FinishReason::Stop => StopReason::EndTurn,
					completions::FinishReason::Length => StopReason::MaxTokens,
					completions::FinishReason::ToolCalls | completions::FinishReason::FunctionCall => {
						StopReason::ToolUse
					},
					completions::FinishReason::ContentFilter => StopReason::Refusal,
				};
				// A named stop sequence is `stop_sequence`, not `end_turn`.
				if stop_reason == StopReason::EndTurn
					&& let Some(sequence) =
						crate::conversion::completions::from_messages::choice_stop_sequence(&choice.rest)
				{
					stop_reason = StopReason::StopSequence;
					state.stop_sequence = Some(sequence);
				}
				state.stop_reason = Some(stop_reason);
			}
		}
		state.end(&mut out, &log, false, log_content.tool_calls);
		out
	}
}

/// Writes events as a Completions client stream, ended by `[DONE]` unless the stream fails.
pub fn render_stream<I: DeserializeOwned + 'static>(
	body: Body,
	buffer_limit: usize,
	mut parse: impl FnMut(SseJsonEvent<I>) -> Vec<StreamEvent> + Send + 'static,
) -> Body {
	struct ToolCall {
		index: u32,
		sent_arguments: bool,
	}
	#[derive(Default)]
	struct Chunks {
		id: Option<String>,
		model: String,
		service_tier: Option<String>,
		created: u32,
		/// Completions puts the role on a choice, so it waits for the first one.
		role: Option<completions::Role>,
		next_tool: u32,
		tool_calls: HashMap<usize, ToolCall>,
	}
	impl Chunks {
		fn chunk(
			&mut self,
			mut choices: Vec<completions::ChatChoiceStream>,
			usage: Option<completions::Usage>,
		) -> completions::ChatStreamEvent {
			if let Some(first) = choices.first_mut() {
				first.delta.role = self.role.take();
			}
			completions::ChatStreamEvent::Chunk(completions::StreamResponse {
				id: self.id.clone().unwrap_or_else(|| "unknown".to_string()),
				model: self.model.clone(),
				object: "chat.completion.chunk".to_string(),
				system_fingerprint: None,
				service_tier: self.service_tier.clone(),
				created: self.created,
				choices,
				usage,
			})
		}

		fn delta(&mut self, delta: completions::StreamResponseDelta) -> completions::ChatStreamEvent {
			self.chunk(vec![completions::ChatChoiceStream::with_delta(delta)], None)
		}

		fn arguments(&mut self, index: u32, arguments: String) -> completions::ChatStreamEvent {
			self.delta(completions::StreamResponseDelta::with_tool_calls(vec![
				completions::ChatCompletionMessageToolCallChunk {
					index,
					id: None,
					r#type: None,
					function: Some(completions::FunctionCallStream {
						name: None,
						arguments: Some(arguments),
					}),
				},
			]))
		}

		fn write(&mut self, event: StreamEvent) -> Option<completions::ChatStreamEvent> {
			match event {
				StreamEvent::Start {
					id,
					model,
					service_tier,
				} => {
					self.id = Some(id);
					self.model = model;
					self.service_tier = service_tier;
					self.role = Some(completions::Role::Assistant);
					None
				},
				StreamEvent::Text(text) => Some(self.delta(completions::StreamResponseDelta {
					content: Some(text),
					..Default::default()
				})),
				StreamEvent::Reasoning(text) => Some(self.delta(completions::StreamResponseDelta {
					reasoning_content: Some(text),
					..Default::default()
				})),
				StreamEvent::Signature(signature) => Some(self.delta(completions::StreamResponseDelta {
					reasoning_signature: Some(signature),
					..Default::default()
				})),
				StreamEvent::ToolCallStart { index, id, name } => {
					let tool = self.next_tool;
					self.next_tool += 1;
					self.tool_calls.insert(
						index,
						ToolCall {
							index: tool,
							sent_arguments: false,
						},
					);
					Some(
						self.delta(completions::StreamResponseDelta::with_tool_calls(vec![
							completions::ChatCompletionMessageToolCallChunk {
								index: tool,
								id: Some(id),
								r#type: Some(completions::FunctionType::Function),
								function: Some(completions::FunctionCallStream {
									name: Some(name),
									arguments: None,
								}),
							},
						])),
					)
				},
				StreamEvent::ToolCallArguments { index, json } => {
					let call = self
						.tool_calls
						.get_mut(&index)
						.filter(|_| !json.is_empty())?;
					call.sent_arguments = true;
					let tool = call.index;
					Some(self.arguments(tool, json))
				},
				// A call that sent no arguments gets `{}`, which clients can parse.
				StreamEvent::ToolCallEnd { index } => {
					let call = self
						.tool_calls
						.remove(&index)
						.filter(|call| !call.sent_arguments)?;
					Some(self.arguments(call.index, "{}".to_string()))
				},
				StreamEvent::End {
					stop_reason, usage, ..
				} => {
					let choices = stop_reason
						.map(|reason| {
							vec![completions::ChatChoiceStream::with_finish_reason(
								finish_reason(reason),
							)]
						})
						.unwrap_or_default();
					let usage = usage.map(|usage| completions::Usage {
						prompt_tokens: usage.input_tokens as u32,
						completion_tokens: usage.output_tokens as u32,
						total_tokens: usage.input_tokens.saturating_add(usage.output_tokens) as u32,
						cache_read_input_tokens: usage.cache_read_tokens,
						prompt_tokens_details: completions::UsagePromptDetails::from_cache_tokens(
							usage.cache_read_tokens,
							usage.cache_write_tokens,
						),
						cache_creation_input_tokens: usage.cache_write_tokens,
						..Default::default()
					});
					Some(self.chunk(choices, usage))
				},
				StreamEvent::Error { kind, message } => Some(completions::ChatStreamEvent::Error(
					crate::conversion::messages::from_completions::stream_error(MessagesError {
						r#type: kind,
						message,
					}),
				)),
			}
		}
	}

	let mut chunks = Chunks {
		created: chrono::Utc::now().timestamp() as u32,
		..Default::default()
	};
	let body = crate::parse::sse::json_transform_multi::<I, completions::ChatStreamEvent, _>(
		body,
		buffer_limit,
		move |event| {
			parse(event)
				.into_iter()
				.filter_map(|event| chunks.write(event))
				.map(|chunk| ("", chunk))
				.collect::<Vec<_>>()
		},
	);
	crate::parse::sse::append_done_on_success(body)
}

fn finish_reason(reason: StopReason) -> completions::FinishReason {
	match reason {
		StopReason::EndTurn | StopReason::StopSequence | StopReason::PauseTurn => {
			completions::FinishReason::Stop
		},
		StopReason::MaxTokens | StopReason::ModelContextWindowExceeded => {
			completions::FinishReason::Length
		},
		StopReason::ToolUse => completions::FinishReason::ToolCalls,
		StopReason::Refusal => completions::FinishReason::ContentFilter,
	}
}
