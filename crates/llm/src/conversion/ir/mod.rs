//! A format-neutral chat request and buffered reply. Each format parses its typed wire structs
//! into these types and renders them back out, so a new format only needs to convert to and from
//! the IR rather than to every other format.
//!
//! Parsing keeps what any renderer needs; each renderer applies the rules of its own format.

use std::collections::BTreeMap;

use serde_json::Value;

pub use crate::types::messages::typed::StopReason;

pub mod completions;
pub mod messages;
pub mod responses;

pub struct Request {
	/// `None` when a Completions or Responses request names none.
	pub model: Option<String>,
	/// The system prompt, apart from any system turns: Messages `system` as `Role::System`, and
	/// Responses `instructions` as `Role::Developer`.
	pub instructions: Option<Message>,
	/// The conversation in order. Completions `system` and `developer` messages, and Messages
	/// `system` turns, stay where they are. Messages has one system prompt, so its renderer
	/// gathers the instructions and every system turn into it.
	pub messages: Vec<Message>,
	/// Function tools only: other tools are dropped. `None` when the client sent no tool list.
	/// Messages renders an empty list as `tools: []`, and only adds a tool choice for
	/// `parallel_tool_calls` when there is a tool; Completions leaves an empty list out.
	pub tools: Option<Vec<Tool>>,
	pub tool_choice: Option<ToolChoice>,
	/// Completions and Responses `parallel_tool_calls`. On Messages it is the inverse of the tool
	/// choice's `disable_parallel_tool_use`, so it is set only when the client set a choice
	/// carrying it.
	pub parallel_tool_calls: Option<bool>,
	/// The output limit. Completions `max_completion_tokens`, else the deprecated `max_tokens`;
	/// Messages `max_tokens`, which it requires, so its renderer defaults it to 4096; Responses
	/// `max_output_tokens`.
	pub max_tokens: Option<usize>,
	pub temperature: Option<f32>,
	pub top_p: Option<f32>,
	/// Completions `stop`, a string or a list; Messages `stop_sequences`. Responses has none.
	pub stop: Vec<String>,
	/// False when absent. A streamed Completions request is rendered asking for usage in the
	/// stream.
	pub stream: bool,
	/// Messages `metadata`. Completions `user` is its `user_id`, the only field Completions renders.
	pub metadata: BTreeMap<String, String>,
	/// Messages `thinking`. Completions has no such field: only an internally set budget becomes
	/// `Enabled`. Messages renders a budget or adaptive thinking as the model's catalog entry
	/// allows; a budget is capped below `max_tokens`, and dropped when it is under 1024 or
	/// `max_tokens` is 1024 or less. Completions and Responses render it as an effort: `Disabled`
	/// sends none, and `Adaptive` or a budget without an effort is mapped to one.
	pub thinking: Option<Thinking>,
	/// Messages `output_config.effort`, Completions `reasoning_effort` and Responses
	/// `reasoning.effort`, where `none` is no effort. Messages has no `minimal`, so it renders
	/// `low`. Completions drops it for models that take no effort.
	pub effort: Option<Effort>,
	/// Completions `response_format` and Responses `text.format`, where `text` is none; Messages
	/// `output_config.format`.
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
		if caching.cache_system
			&& caching.min_tokens.is_none_or(|min| {
				let system = self.system().flat_map(Message::texts);
				crate::conversion::estimate_tokens(system) >= min
			}) {
			let mut system: Vec<_> = self
				.instructions
				.iter_mut()
				.chain(self.messages.iter_mut().filter(|msg| msg.role.is_system()))
				.collect();
			system.iter_mut().rev().any(|msg| msg.mark_last());
		}
		if caching.cache_messages {
			// Counted as rendered: system instructions move out of the turns, and empty turns go.
			let mut turns: Vec<_> = self
				.messages
				.iter_mut()
				.filter(|msg| !msg.role.is_system() && msg.has_content())
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

	/// The instructions, then each system turn.
	pub fn system(&self) -> impl Iterator<Item = &Message> {
		self
			.instructions
			.iter()
			.chain(self.messages.iter().filter(|msg| msg.role.is_system()))
	}

	fn has_cache_marks(&self) -> bool {
		fn marked(content: &Content) -> bool {
			let Content::Parts(parts) = content else {
				return false;
			};
			parts.iter().any(|part| match part {
				// Empty text can't hold a mark, so a mark on it counts for nothing.
				Part::Text { text, cache } => *cache && !text.trim().is_empty(),
				Part::Image { cache, .. }
				| Part::Document { cache, .. }
				| Part::ToolCall { cache, .. }
				| Part::Opaque { cache } => *cache,
				Part::ToolResult { content, cache, .. } => *cache || marked(content),
				Part::Reasoning { .. } => false,
			})
		}
		self
			.instructions
			.iter()
			.chain(&self.messages)
			.any(|msg| marked(&msg.content))
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
			| Part::Document { cache, .. }
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
	/// OpenAI's newer name for system instructions, which its formats keep apart.
	Developer,
	User,
	Assistant,
}

impl Role {
	pub fn is_system(self) -> bool {
		matches!(self, Role::System | Role::Developer)
	}
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
	Image {
		source: Source,
		cache: bool,
	},
	/// A file, such as a PDF.
	Document {
		source: Source,
		title: Option<String>,
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

/// Where an image or document's content is.
pub enum Source {
	/// A URL, which may be a `data:` URL holding the content.
	Url(String),
	/// A file the provider stores.
	FileId(String),
	/// Plain text, for a document.
	Text(String),
	/// A source the client sent that can't be read, and why. A renderer that can't skip it
	/// refuses the request with this reason.
	Invalid(&'static str),
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Effort {
	Minimal,
	Low,
	Medium,
	High,
	Xhigh,
	Max,
}

pub enum Thinking {
	Enabled { budget_tokens: u64 },
	Adaptive,
	Disabled,
}

pub enum ResponseFormat {
	/// A schema, with the name, description and strictness Completions and Responses give it.
	JsonSchema {
		name: Option<String>,
		description: Option<String>,
		schema: Value,
		strict: Option<bool>,
	},
	/// Completions and Responses `json_object`. Messages has no JSON mode, so it renders a schema
	/// for any object.
	JsonObject,
}

pub struct Response {
	pub id: String,
	pub model: String,
	pub parts: Vec<ReplyPart>,
	pub stop_reason: Option<StopReason>,
	pub stop_sequence: Option<String>,
	/// `None` when the provider reported none.
	pub usage: Option<Usage>,
	pub service_tier: Option<String>,
	/// The provider's own counts, for accounting.
	provider_usage: Option<super::ProviderUsage>,
}

/// A piece of a reply, in order.
pub enum ReplyPart {
	Text {
		text: String,
		citations: Vec<Citation>,
	},
	Refusal(String),
	Reasoning {
		text: String,
		signature: String,
	},
	ToolCall {
		id: String,
		name: String,
		/// The arguments as JSON text.
		arguments: String,
		/// Gemini's signature for the call, which must be replayed with it.
		thought_signature: Option<String>,
	},
}

/// A web source the text cites.
pub struct Citation {
	pub url: Value,
	pub title: Value,
}

#[derive(Default)]
pub struct Usage {
	/// Every input token, cached ones included.
	pub input_tokens: u64,
	pub output_tokens: u64,
	/// As the provider reported it, when it did.
	pub total_tokens: Option<u64>,
	pub reasoning_tokens: Option<u64>,
	pub cache_read_tokens: Option<u64>,
	pub cache_write_tokens: Option<u64>,
	pub input_audio_tokens: Option<u64>,
	pub output_audio_tokens: Option<u64>,
}

/// A streamed reply, as a sequence of these. A provider stream parses into them and a client
/// stream renders from them. Each renderer buffers what its format needs whole.
pub enum StreamEvent {
	Start {
		id: String,
		model: String,
		service_tier: Option<String>,
	},
	/// A text part begins before any of its text.
	TextStart {
		index: usize,
	},
	/// More of text part `index`, which may be empty.
	Text {
		index: usize,
		text: String,
	},
	TextEnd {
		index: usize,
	},
	Reasoning(String),
	/// Attests to the reasoning so far.
	Signature(String),
	/// More of tool call `index`: its id and name as they arrive, and more arguments as JSON
	/// text, which may be empty.
	ToolCall {
		index: usize,
		id: Option<String>,
		name: Option<String>,
		arguments: String,
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
