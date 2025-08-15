pub use async_openai::types::{
	ChatChoice, ChatChoiceStream, ChatCompletionMessageToolCall as MessageToolCall,
	ChatCompletionNamedToolChoice as NamedToolChoice,
	ChatCompletionRequestAssistantMessage as RequestAssistantMessage,
	ChatCompletionRequestAssistantMessageContent as RequestAssistantMessageContent,
	ChatCompletionRequestDeveloperMessage as RequestDeveloperMessage,
	ChatCompletionRequestDeveloperMessageContent as RequestDeveloperMessageContent,
	ChatCompletionRequestFunctionMessage as RequestFunctionMessage,
	ChatCompletionRequestMessage as RequestMessage,
	ChatCompletionRequestSystemMessage as RequestSystemMessage,
	ChatCompletionRequestSystemMessageContent as RequestSystemMessageContent,
	ChatCompletionRequestToolMessage as RequestToolMessage,
	ChatCompletionRequestToolMessageContent as RequestToolMessageContent,
	ChatCompletionRequestUserMessage as RequestUserMessage,
	ChatCompletionRequestUserMessageContent as RequestUserMessageContent,
	ChatCompletionResponseMessage as ResponseMessage, ChatCompletionStreamOptions as StreamOptions,
	ChatCompletionStreamResponseDelta as StreamResponseDelta,
	ChatCompletionToolChoiceOption as ToolChoiceOption, ChatCompletionToolType as ToolType,
	CompletionUsage as Usage, CreateChatCompletionRequest as Request,
	CreateChatCompletionResponse as Response, CreateChatCompletionStreamResponse as StreamResponse,
	FinishReason, FunctionCall, Role,
};
use async_openai::types::{CreateChatCompletionRequest, Stop};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct ChatCompletionErrorResponse {
	pub event_id: Option<String>,
	pub error: ChatCompletionError,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ChatCompletionError {
	pub r#type: String,
	pub message: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub param: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub code: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub event_id: Option<String>,
}

pub const SYSTEM_ROLE: &str = "system";
pub const ASSISTANT_ROLE: &str = "assistant";

pub fn message_role(msg: &RequestMessage) -> &'static str {
	match msg {
		RequestMessage::Developer(_) => "developer",
		RequestMessage::System(_) => "system",
		RequestMessage::Assistant(_) => "assistant",
		RequestMessage::Tool(_) => "tool",
		RequestMessage::Function(_) => "function",
		RequestMessage::User(_) => "user",
	}
}

pub fn message_name(msg: &RequestMessage) -> Option<&str> {
	match msg {
		RequestMessage::Developer(RequestDeveloperMessage { name, .. }) => name.as_deref(),
		RequestMessage::System(RequestSystemMessage { name, .. }) => name.as_deref(),
		RequestMessage::Assistant(RequestAssistantMessage { name, .. }) => name.as_deref(),
		RequestMessage::Tool(RequestToolMessage { tool_call_id, .. }) => Some(tool_call_id.as_str()),
		RequestMessage::Function(RequestFunctionMessage { name, .. }) => Some(name.as_str()),
		RequestMessage::User(RequestUserMessage { name, .. }) => name.as_deref(),
	}
}

pub fn message_text(msg: &RequestMessage) -> Option<&str> {
	// All of these types support Vec<Text>... show we support those?
	// Right now, we don't support
	match msg {
		RequestMessage::Developer(RequestDeveloperMessage {
			content: RequestDeveloperMessageContent::Text(t),
			..
		}) => Some(t.as_str()),
		RequestMessage::System(RequestSystemMessage {
			content: RequestSystemMessageContent::Text(t),
			..
		}) => Some(t.as_str()),
		RequestMessage::Assistant(RequestAssistantMessage {
			content: Some(RequestAssistantMessageContent::Text(t)),
			..
		}) => Some(t.as_str()),
		RequestMessage::Tool(RequestToolMessage {
			content: RequestToolMessageContent::Text(t),
			..
		}) => Some(t.as_str()),
		RequestMessage::User(RequestUserMessage {
			content: RequestUserMessageContent::Text(t),
			..
		}) => Some(t.as_str()),
		_ => None,
	}
}

pub fn max_tokens(req: &CreateChatCompletionRequest) -> usize {
	#![allow(deprecated)]
	req.max_completion_tokens.or(req.max_tokens).unwrap_or(4096) as usize
}

pub fn max_tokens_option(req: &CreateChatCompletionRequest) -> Option<u64> {
	#![allow(deprecated)]
	req.max_completion_tokens.or(req.max_tokens).map(Into::into)
}

pub fn stop_sequence(req: &CreateChatCompletionRequest) -> Vec<String> {
	match &req.stop {
		Some(Stop::String(s)) => vec![s.clone()],
		Some(Stop::StringArray(s)) => s.clone(),
		_ => vec![],
	}
}
