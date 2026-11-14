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
	/// Empty when a Completions request names none.
	pub model: String,
	/// The conversation in order, system instructions included. Completions `system` and
	/// `developer` messages stay where they are, as `Role::System`. A Messages top-level `system`
	/// becomes the first message, and a Messages `system` turn keeps its place. Messages has one
	/// top-level system prompt, so its renderer gathers every system message into it.
	pub messages: Vec<Message>,
	/// Function tools only: Completions custom tools and Messages server tools are dropped.
	/// `None` when the client sent no tool list. Messages renders an empty list as `tools: []`,
	/// and only adds a tool choice for `parallel_tool_calls` when there is a tool.
	pub tools: Option<Vec<Tool>>,
	pub tool_choice: Option<ToolChoice>,
	/// Completions `parallel_tool_calls`. On Messages it is the inverse of the tool choice's
	/// `disable_parallel_tool_use`, so it is set only when the client set a choice carrying it.
	pub parallel_tool_calls: Option<bool>,
	/// The output limit, which Messages requires. On Completions it is `max_completion_tokens`,
	/// else the deprecated `max_tokens`, else 4096; Completions renders `max_completion_tokens`.
	pub max_tokens: usize,
	pub temperature: Option<f32>,
	pub top_p: Option<f32>,
	/// Completions `stop`, a string or a list; Messages `stop_sequences`.
	pub stop: Vec<String>,
	/// Completions `stream`, false when absent; Messages `stream`. A streamed Completions request
	/// is rendered asking for usage in the stream.
	pub stream: bool,
	/// Completions `user`; Messages `metadata.user_id`. Other Messages metadata is dropped.
	pub user: Option<String>,
	/// Messages `thinking`. Completions has no such field: only an internally set budget becomes
	/// `Enabled`. Messages renders a budget or adaptive thinking as the model's catalog entry
	/// allows; a budget is capped below `max_tokens`, and dropped when it is under 1024 or
	/// `max_tokens` is 1024 or less. Completions renders it as `reasoning_effort` with `effort`:
	/// `Disabled` sends none, and `Adaptive` or a budget without an effort is mapped to one.
	pub thinking: Option<Thinking>,
	/// On Anthropic's scale. Messages `output_config.effort`. Completions `reasoning_effort`, where
	/// `minimal` becomes `low` and `none` is no effort.
	pub effort: Option<Effort>,
	/// Completions `response_format`, where `text` is none; Messages `output_config.format`. Only
	/// a JSON schema is kept, not its name or strictness: Completions renders it named
	/// `structured_output`.
	pub response_format: Option<ResponseFormat>,
}

impl Request {
	/// Applies a prompt caching policy: marks the system prompt, the last part of a turn in the
	/// history, and the last tool. A client that marked anything keeps its own marks and gets
	/// none added. Renderers drop the marks their format can't carry.
	pub fn apply_prompt_caching(&mut self, caching: &crate::PromptCachingConfig) {
		if self.has_cache_marks() {
			return;
		}
		let system = |msg: &&mut Message| msg.role == Role::System;
		if caching.cache_system
			&& caching.min_tokens.is_none_or(|min| {
				let texts = self.messages.iter().filter(|msg| msg.role == Role::System);
				crate::conversion::estimate_tokens(texts.flat_map(Message::texts)) >= min
			}) {
			self
				.messages
				.iter_mut()
				.rev()
				.filter(system)
				.any(Message::mark_last);
		}
		if caching.cache_messages {
			// Counted as rendered: system instructions move out of the turns, and empty turns go.
			let mut turns: Vec<_> = self
				.messages
				.iter_mut()
				.filter(|msg| msg.role != Role::System && msg.has_content())
				.collect();
			if turns.len() >= 2 {
				let at = (turns.len() - 2).saturating_sub(caching.cache_message_offset);
				turns[at].mark_last();
			}
		}
		if caching.cache_tools
			&& let Some(tool) = self.tools.iter_mut().flatten().last()
		{
			tool.cache = true;
		}
	}

	fn has_cache_marks(&self) -> bool {
		fn marked(content: &Content) -> bool {
			let Content::Parts(parts) = content else {
				return false;
			};
			parts.iter().any(|part| match part {
				Part::Text { cache, .. }
				| Part::Image { cache, .. }
				| Part::File { cache, .. }
				| Part::ToolCall { cache, .. }
				| Part::Opaque { cache } => *cache,
				Part::ToolResult { content, cache, .. } => *cache || marked(content),
				Part::Reasoning { .. } => false,
			})
		}
		self.messages.iter().any(|msg| marked(&msg.content))
			|| self.tools.iter().flatten().any(|tool| tool.cache)
	}
}

impl Message {
	fn texts(&self) -> Vec<&str> {
		match &self.content {
			Content::Text(text) => vec![text],
			Content::Parts(parts) => parts
				.iter()
				.filter_map(|part| match part {
					Part::Text { text, .. } => Some(text.as_str()),
					_ => None,
				})
				.collect(),
		}
	}

	/// Marks the last part that can carry a cache mark: not empty text or reasoning. Returns
	/// whether there was one.
	fn mark_last(&mut self) -> bool {
		let parts = match &mut self.content {
			Content::Text(text) if !text.trim().is_empty() => {
				let text = std::mem::take(text);
				self.content = Content::Parts(vec![Part::Text { text, cache: true }]);
				return true;
			},
			Content::Text(_) => return false,
			Content::Parts(parts) => parts,
		};
		let last = parts.iter_mut().rev().find_map(|part| match part {
			Part::Text { text, cache } if !text.trim().is_empty() => Some(cache),
			Part::Image { cache, .. }
			| Part::File { cache, .. }
			| Part::ToolCall { cache, .. }
			| Part::ToolResult { cache, .. } => Some(cache),
			_ => None,
		});
		match last {
			Some(cache) => {
				*cache = true;
				true
			},
			None => false,
		}
	}

	fn has_content(&self) -> bool {
		match &self.content {
			Content::Text(text) => !text.trim().is_empty(),
			Content::Parts(parts) => parts.iter().any(|part| match part {
				Part::Text { text, .. } => !text.trim().is_empty(),
				Part::Opaque { .. } => false,
				_ => true,
			}),
		}
	}
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
		cache: bool,
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
	pub cache: bool,
}

pub enum ToolChoice {
	Auto,
	/// Completions `required`; Messages `any`.
	Required,
	None,
	Tool(String),
	/// A choice the source format has but the IR does not model, such as Completions
	/// `allowed_tools`. It renders as no choice.
	Other,
}

pub enum Thinking {
	Enabled { budget_tokens: u64 },
	Adaptive,
	Disabled,
}

pub enum ResponseFormat {
	JsonSchema(Value),
	/// Completions `json_object`. It renders as a schema for any object, since Messages has no
	/// JSON mode.
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

/// A streamed reply, as a sequence of these. A provider stream parses into them and a client
/// stream renders from them.
pub enum StreamEvent {
	Start {
		id: String,
		model: String,
		service_tier: Option<String>,
	},
	Text(String),
	Reasoning(String),
	/// Attests to the reasoning so far.
	Signature(String),
	/// A tool call begins. `index` identifies it among the reply's calls.
	ToolCallStart {
		index: usize,
		id: String,
		name: String,
	},
	/// More of a tool call's arguments, as JSON text, which may be empty.
	ToolCallArguments {
		index: usize,
		json: String,
	},
	/// A tool call's arguments are complete. Completions doesn't say, so its calls never end.
	ToolCallEnd {
		index: usize,
	},
	/// The reply ends. Usage is the total. A Messages provider may send no stop reason.
	End {
		stop_reason: Option<StopReason>,
		stop_sequence: Option<String>,
		usage: Option<Usage>,
	},
	/// The provider failed mid-stream, with a Messages error type.
	Error {
		kind: String,
		message: String,
	},
}
