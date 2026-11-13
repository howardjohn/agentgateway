//! A format-neutral chat request and buffered reply. Each format parses its typed wire structs
//! into these types and renders them back out, so a new format only needs to convert to and from
//! the IR rather than to every other format.
//!
//! Parsing keeps what any renderer needs; each renderer applies the rules of its own format.

use serde_json::Value;

pub use crate::types::messages::typed::{StopReason, ThinkingEffort as Effort};

pub mod completions;
pub mod messages;

pub struct Request {
	pub model: String,
	pub messages: Vec<Message>,
	/// `None` when the client sent no tool list, which some renderers treat differently from an
	/// empty one.
	pub tools: Option<Vec<Tool>>,
	pub tool_choice: Option<ToolChoice>,
	pub parallel_tool_calls: Option<bool>,
	pub max_tokens: usize,
	pub temperature: Option<f32>,
	pub top_p: Option<f32>,
	pub stop: Vec<String>,
	pub stream: bool,
	pub user: Option<String>,
	pub thinking: Option<Thinking>,
	pub effort: Option<Effort>,
	pub response_format: Option<ResponseFormat>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
	System,
	User,
	Assistant,
}

pub struct Message {
	pub role: Role,
	pub content: Content,
}

/// Formats distinguish a plain string from a list of parts, and some render them differently.
pub enum Content {
	Text(String),
	Parts(Vec<Part>),
}

pub enum Part {
	Text {
		text: String,
		cache: bool,
	},
	/// A URL, which may be a `data:` URL.
	Image {
		url: String,
		cache: bool,
	},
	File {
		data: Option<String>,
		id: Option<String>,
		name: Option<String>,
		cache: bool,
	},
	ToolCall {
		id: String,
		name: String,
		/// The arguments as JSON text.
		arguments: String,
		/// Gemini's signature for the call, which must be replayed with it.
		thought_signature: Option<String>,
	},
	ToolResult {
		id: String,
		content: Content,
		is_error: Option<bool>,
		cache: bool,
	},
	Reasoning {
		text: String,
		signature: String,
	},
	/// Content no renderer can carry, kept only for its cache mark.
	Opaque {
		cache: bool,
	},
}

pub struct Tool {
	pub name: String,
	pub description: Option<String>,
	pub parameters: Option<Value>,
	pub strict: Option<bool>,
}

pub enum ToolChoice {
	Auto,
	Required,
	None,
	Tool(String),
	/// A choice the source format has but the IR does not model.
	Other,
}

pub enum Thinking {
	Enabled { budget_tokens: u64 },
	Adaptive,
	Disabled,
}

pub enum ResponseFormat {
	JsonSchema(Value),
	JsonObject,
}

pub struct Response {
	pub id: String,
	pub model: String,
	/// Text, reasoning and tool calls, in order.
	pub parts: Vec<Part>,
	pub stop_reason: Option<StopReason>,
	pub stop_sequence: Option<String>,
	pub usage: Usage,
	pub service_tier: Option<String>,
	/// The provider's own counts, for accounting.
	provider_usage: Option<super::ProviderUsage>,
}

#[derive(Default)]
pub struct Usage {
	/// Every input token, cached ones included.
	pub input_tokens: u64,
	pub output_tokens: u64,
	pub cache_read_tokens: Option<u64>,
	pub cache_write_tokens: Option<u64>,
	pub input_audio_tokens: Option<u64>,
	pub output_audio_tokens: Option<u64>,
}
