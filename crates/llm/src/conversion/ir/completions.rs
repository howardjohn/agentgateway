//! Chat Completions to and from the IR.

use agent_core::strng;
use bytes::Bytes;

use super::{
	Content, Effort, Message, Part, Request, Response, ResponseFormat, Role, StopReason, Thinking,
	Tool, ToolChoice, Usage,
};
use crate::conversion::{ProviderUsage, ResponseWithProviderUsage};
use crate::types::ResponseType;
use crate::types::completions::typed as completions;
use crate::{AIError, json, logged_response_parsing, types};

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
					id: call.id,
					name: call.function.name,
					arguments: call.function.arguments,
				},
				completions::MessageToolCalls::Custom(call) => Part::ToolCall {
					id: call.id,
					name: call.custom_tool.name,
					arguments: call.custom_tool.input,
					thought_signature: None,
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

	let usage = resp
		.usage
		.map(|u| {
			let prompt = u.prompt_tokens_details.as_ref();
			let completion = u.completion_tokens_details.as_ref();
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
				output_audio_tokens: completion.and_then(|d| d.audio_tokens),
			}
		})
		.unwrap_or_default();

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
	let finish_reason = resp.stop_reason.map(|reason| match reason {
		StopReason::EndTurn | StopReason::StopSequence | StopReason::PauseTurn => {
			completions::FinishReason::Stop
		},
		StopReason::MaxTokens | StopReason::ModelContextWindowExceeded => {
			completions::FinishReason::Length
		},
		StopReason::ToolUse => completions::FinishReason::ToolCalls,
		StopReason::Refusal => completions::FinishReason::ContentFilter,
	});
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
