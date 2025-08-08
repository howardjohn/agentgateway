use agent_core::prelude::Strng;
use agent_core::strng;
use async_anthropic::types::{CreateMessagesResponse, MessageContent};
use bytes::Bytes;
use chrono;
use itertools::Itertools;
use serde::Serialize;
use serde_json::{Value, json};

use self::types::*;
use crate::http::Response;
use crate::llm::universal::{
	ChatCompletionChoiceStream, ChatCompletionRequest, FinishReason, Usage,
};
use crate::llm::{AIError, LLMRequest, LLMResponse, universal};
use crate::telemetry::log::AsyncLog;
use crate::{llm, parse, *};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Provider {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub model: Option<Strng>,
}

impl super::Provider for Provider {
	const NAME: Strng = strng::literal!("anthropic");
}
pub const DEFAULT_HOST_STR: &str = "api.anthropic.com";
pub const DEFAULT_HOST: Strng = strng::literal!(DEFAULT_HOST_STR);
pub const DEFAULT_PATH: &str = "/v1/messages";

impl Provider {
	pub async fn process_request(
		&self,
		mut req: universal::ChatCompletionRequest,
	) -> Result<CreateMessagesRequest, AIError> {
		if let Some(model) = &self.model {
			req.model = model.to_string();
		}
		let anthropic_message = translate_request(req);
		Ok(anthropic_message)
	}
	pub async fn process_response(
		&self,
		bytes: &Bytes,
	) -> Result<universal::ChatCompletionResponse, AIError> {
		let resp =
			serde_json::from_slice::<CreateMessagesResponse>(bytes).map_err(AIError::ResponseParsing)?;
		let openai = translate_response(resp);
		Ok(openai)
	}

	pub async fn process_streaming(&self, log: AsyncLog<LLMResponse>, resp: Response) -> Response {
		resp.map(|b| {
			let mut message_id = None;
			let mut model = String::new();
			let mut created = chrono::Utc::now().timestamp();
			let mut finish_reason = None;
			let mut input_tokens = 0;
			// https://docs.anthropic.com/en/docs/build-with-claude/streaming
			parse::sse::json_transform::<MessagesStreamEvent, universal::ChatCompletionStreamResponse>(
				b,
				move |f| {
					let mk = |choices: Vec<ChatCompletionChoiceStream>, usage: Option<Usage>| {
						Some(universal::ChatCompletionStreamResponse {
							id: message_id.clone(),
							model: model.clone(),
							object: "chat.completion.chunk".to_string(),
							system_fingerprint: None,
							created,
							choices,
							usage,
						})
					};
					// ignore errors... what else can we do?
					let f = f.ok()?;

					// Extract info we need
					match f {
						MessagesStreamEvent::MessageStart { message, usage } => {
							message_id = Some(message.id);
							model = message.model.clone();
							if let Some(usage) = usage {
								input_tokens = usage.input_tokens.unwrap_or(0);
								log.non_atomic_mutate(|r| {
									r.output_tokens = Some(usage.output_tokens.unwrap_or(0) as u64);
									r.input_tokens_from_response = Some(usage.input_tokens.unwrap_or(0) as u64);
									r.provider_model = Some(strng::new(&message.model))
								});
							}
							// no need to respond with anything yet
							None
						},

						MessagesStreamEvent::ContentBlockStart { .. } => {
							// There is never(?) any content here
							None
						},
						MessagesStreamEvent::ContentBlockDelta { delta, .. } => {
							// TODO: support input JSON
							if let ContentBlockDelta::TextDelta { text } = delta {
								let choice = universal::ChatCompletionChoiceStream {
									index: 0,
									delta: universal::ChatCompletionMessageForResponseDelta {
										role: None,
										content: Some(text),
										refusal: None,
										name: None,
										tool_calls: None,
									},
									finish_reason: None,
								};
								mk(vec![choice], None)
							} else {
								None
							}
						},
						MessagesStreamEvent::MessageDelta { usage, delta } => {
							finish_reason = to_finish_reason(delta.stop_reason);
							if let Some(usage) = usage {
								log.non_atomic_mutate(|r| {
									r.output_tokens = Some(usage.output_tokens.unwrap_or(0) as u64);
									if let Some(inp) = r.input_tokens_from_response {
										r.total_tokens = Some(inp + usage.output_tokens.unwrap_or(0) as u64)
									}
								});
								mk(
									vec![],
									Some(universal::Usage {
										prompt_tokens: usage.output_tokens.unwrap_or(0) as i32,
										completion_tokens: input_tokens as i32,
										total_tokens: (input_tokens + usage.output_tokens.unwrap_or(0)) as i32,
									}),
								)
							} else {
								None
							}
						},
						MessagesStreamEvent::ContentBlockStop { .. } => None,
						MessagesStreamEvent::MessageStop { .. } => None,
					}
				},
			)
		})
	}

	pub async fn process_error(
		&self,
		bytes: &Bytes,
	) -> Result<universal::ChatCompletionErrorResponse, AIError> {
		let resp =
			serde_json::from_slice::<MessagesErrorResponse>(bytes).map_err(AIError::ResponseParsing)?;
		translate_error(resp)
	}
}

pub(super) fn translate_error(
	resp: MessagesErrorResponse,
) -> Result<universal::ChatCompletionErrorResponse, AIError> {
	Ok(universal::ChatCompletionErrorResponse {
		event_id: None,
		error: universal::ChatCompletionError {
			r#type: "invalid_request_error".to_string(),
			message: resp.error.message,
			param: None,
			code: None,
			event_id: None,
		},
	})
}

pub(super) fn translate_response(
	resp: CreateMessagesResponse,
) -> universal::ChatCompletionResponse {
	// Convert Anthropic content blocks to OpenAI message content
	let mut tool_calls: Vec<universal::ToolCall> = Vec::new();
	let mut content = None;
	for block in resp.content.iter().flatten() {
		match block {
			MessageContent::Text(Text { text }) => content = Some(text.clone()),
			MessageContent::ToolUse(ToolUse { id, name, input }) => {
				let Some(args) = serde_json::to_string(&input).ok() else {
					continue;
				};
				tool_calls.push(universal::ToolCall {
					id: id.clone(),
					r#type: universal::ToolType::Function,
					function: universal::ToolCallFunction {
						name: name.clone(),
						arguments: args,
					},
				});
			},
			MessageContent::ToolResult(_) => {
				// Should be on the request path, not the response path
				continue;
			},
			_ => continue, // Skip images in response for now
		}
	}
	let message = universal::ChatCompletionMessageForResponse {
		role: universal::MessageRole::assistant,
		content,
		tool_calls: if tool_calls.is_empty() {
			None
		} else {
			Some(tool_calls)
		},
	};
	let finish_reason = to_finish_reason(resp.stop_reason);
	// Only one choice for anthropic
	let choice = universal::ChatCompletionChoice {
		index: 0,
		message,
		finish_reason,
		finish_details: None,
	};

	let choices = vec![choice];
	// Convert usage from Anthropic format to OpenAI format
	let usage = if let Some(u) = resp.usage {
		// Per API docs, this is required
		universal::Usage {
			prompt_tokens: u.input_tokens.unwrap_or_default() as i32,
			completion_tokens: u.output_tokens.unwrap_or_default() as i32,
			total_tokens: (u.input_tokens.unwrap_or_default() + u.output_tokens.unwrap_or_default())
				as i32,
		}
	} else {
		universal::Usage::default()
	};

	universal::ChatCompletionResponse {
		id: resp.id,
		object: "chat.completion".to_string(),
		// No date in anthropic response so just call it "now"
		created: chrono::Utc::now().timestamp(),
		// Per API docs, this is required
		model: resp.model.unwrap_or_default(),
		choices,
		usage,
		system_fingerprint: None,
	}
}

pub(super) fn translate_request(req: ChatCompletionRequest) -> CreateMessagesRequest {
	// Anthropic has all system prompts in a single field. Join them
	let system = req
		.messages
		.iter()
		.filter_map(|msg| {
			if msg.role == universal::MessageRole::system {
				match &msg.content {
					universal::Content::Text(text) => Some(text.clone()),
					_ => None, // Skip non-text system messages
				}
			} else {
				None
			}
		})
		.collect::<Vec<String>>()
		.join("\n");

	// Convert messages to Anthropic format
	let messages = req
		.messages
		.iter()
		.filter(|msg| msg.role != universal::MessageRole::system)
		.map(|msg| {
			let role = match msg.role {
				universal::MessageRole::user => MessageRole::User,
				universal::MessageRole::assistant => MessageRole::Assistant,
				_ => MessageRole::User, // Default to user for other roles
			};

			let content = match &msg.content {
				universal::Content::Text(text) => {
					vec![MessageContent::Text(Text { text: text.clone() })]
				},
				universal::Content::ImageUrl(urls) => {
					// TODO: support image
					vec![]
				},
			};

			Message {
				role,
				content: MessageContentList(content),
			}
		})
		.collect();

	let tools = if let Some(tools) = req.tools {
		let mapped_tools: Vec<_> = tools
			.iter()
			.filter_map(|tool| {
				let t = json!({
					"name": tool.function.name.clone(),
					"description": tool.function.description.clone(),
					"input_schema": tool.function.parameters.clone().unwrap_or_default(),
				});
				if let serde_json::Value::Object(m) = t {
					Some(m)
				} else {
					None
				}
			})
			.collect();
		Some(mapped_tools)
	} else {
		None
	};
	let metadata = req.user.and_then(|user| {
		let t = json!({
			"user_id": Some(user),
		});
		if let serde_json::Value::Object(m) = t {
			Some(m)
		} else {
			None
		}
	});

	let tool_choice = match req.tool_choice {
		Some(universal::ToolChoiceType::ToolChoice { r#type, function }) => {
			Some(types::ToolChoice::Tool(function.name))
		},
		Some(universal::ToolChoiceType::Auto) => Some(types::ToolChoice::Auto),
		Some(universal::ToolChoiceType::Required) => Some(types::ToolChoice::Any),
		Some(universal::ToolChoiceType::None) => None,
		None => None,
	};
	CreateMessagesRequest {
		messages,
		system: Some(system),
		model: req.model,
		max_tokens: req.max_tokens.unwrap_or(4096) as i32,
		stop_sequences: Some(req.stop.unwrap_or_default()),
		stream: req.stream.unwrap_or_default(),
		temperature: req.temperature.map(|f| f as f32),
		top_p: req.top_p.map(|f| f as f32),
		top_k: None, // OpenAI doesn't have top_k
		tools,
		tool_choice,
		metadata,
	}
}

fn to_finish_reason(reason: Option<String>) -> Option<FinishReason> {
	reason.map(|reason| match reason.as_str() {
		MAX_TOKENS => universal::FinishReason::length,
		TOOL_USE => universal::FinishReason::tool_calls,
		REFUSAL => universal::FinishReason::content_filter,
		END_TURN | STOP_SEQUENCE | PAUSE_TURN | _ => universal::FinishReason::stop,
	})
}

pub mod types {
	pub use async_anthropic::types::*;
	use serde::Deserialize;

	pub const END_TURN: &'static str = "end_turn";
	pub const MAX_TOKENS: &'static str = "max_tokens";
	pub const STOP_SEQUENCE: &'static str = "stop_sequence";
	pub const TOOL_USE: &'static str = "tool_use";
	pub const PAUSE_TURN: &'static str = "pause_turn";
	pub const REFUSAL: &'static str = "refusal";

	#[derive(Debug, Deserialize, Clone)]
	pub struct MessagesErrorResponse {
		pub r#type: String,
		pub error: MessagesError,
	}

	#[derive(Debug, Deserialize, Clone)]
	pub struct MessagesError {
		pub r#type: String,
		pub message: String,
	}
}
