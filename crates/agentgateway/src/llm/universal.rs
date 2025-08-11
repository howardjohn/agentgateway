pub use async_openai::types::ChatChoice;
pub use async_openai::types::ChatChoiceStream;
pub use async_openai::types::ChatCompletionAudioFormat as AudioFormat;
pub use async_openai::types::ChatCompletionAudioVoice as AudioVoice;
pub use async_openai::types::ChatCompletionFunctionCall;
pub use async_openai::types::ChatCompletionMessageToolCall as MessageToolCall;
pub use async_openai::types::ChatCompletionModalities as Modalities;
pub use async_openai::types::ChatCompletionNamedToolChoice as NamedToolChoice;
pub use async_openai::types::ChatCompletionRequestAssistantMessage as RequestAssistantMessage;
pub use async_openai::types::ChatCompletionRequestAssistantMessageContent as RequestAssistantMessageContent;
pub use async_openai::types::ChatCompletionRequestAssistantMessageContentPart as RequestAssistantMessageContentPart;
pub use async_openai::types::ChatCompletionRequestDeveloperMessage as RequestDeveloperMessage;
pub use async_openai::types::ChatCompletionRequestDeveloperMessageContent as RequestDeveloperMessageContent;
pub use async_openai::types::ChatCompletionRequestFunctionMessage as RequestFunctionMessage;
pub use async_openai::types::ChatCompletionRequestMessage as RequestMessage;
pub use async_openai::types::ChatCompletionRequestMessageContentPartRefusalBuilderError as RequestMessageContentPartRefusalBuilderError;
pub use async_openai::types::ChatCompletionRequestSystemMessage as RequestSystemMessage;
pub use async_openai::types::ChatCompletionRequestSystemMessageContent as RequestSystemMessageContent;
pub use async_openai::types::ChatCompletionRequestSystemMessageContentPart as RequestSystemMessageContentPart;
pub use async_openai::types::ChatCompletionRequestToolMessage as RequestToolMessage;
pub use async_openai::types::ChatCompletionRequestToolMessageContent as RequestToolMessageContent;
pub use async_openai::types::ChatCompletionRequestToolMessageContentPart as RequestToolMessageContentPart;
pub use async_openai::types::ChatCompletionRequestUserMessage as RequestUserMessage;
pub use async_openai::types::ChatCompletionRequestUserMessageContent as RequestUserMessageContent;
pub use async_openai::types::ChatCompletionRequestUserMessageContentPart as RequestUserMessageContentPart;
pub use async_openai::types::ChatCompletionResponseMessage as ResponseMessage;
pub use async_openai::types::ChatCompletionStreamOptions as StreamOptions;
pub use async_openai::types::ChatCompletionStreamResponseDelta as StreamResponseDelta;
pub use async_openai::types::ChatCompletionToolChoiceOption as ToolChoiceOption;
pub use async_openai::types::ChatCompletionToolType as ToolType;
pub use async_openai::types::CompletionUsage as Usage;
pub use async_openai::types::CreateChatCompletionRequest as Request;
pub use async_openai::types::CreateChatCompletionResponse as Response;
pub use async_openai::types::CreateChatCompletionStreamResponse as StreamResponse;
pub use async_openai::types::FinishReason;
pub use async_openai::types::FunctionCall;
pub use async_openai::types::MessageRole;
pub use async_openai::types::Role;
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

pub const DEVELOPER_ROLE: &str = "developer";
pub const SYSTEM_ROLE: &str = "system";
pub const ASSISTANT_ROLE: &str = "assistant";
pub const TOOL_ROLE: &str = "tool";
pub const FUNCTION_ROLE: &str = "function";
pub const USER_ROLE: &str = "user";

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
			name,
		}) => Some(t.as_str()),
		RequestMessage::Developer(RequestDeveloperMessage {
			content: RequestDeveloperMessageContent::Text(t),
			name,
		}) => Some(t.as_str()),
		RequestMessage::System(RequestSystemMessage {
			content: RequestSystemMessageContent::Text(t),
			name,
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
