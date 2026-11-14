//! Responses to and from the IR.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use agent_core::strng;
use agent_http::Body;
use bytes::Bytes;
use rand::RngExt;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::{
	Citation, Content, Effort, Message, Part, ReplyPart, Request, Response, ResponseFormat, Role,
	Source, StopReason, StreamEvent, Thinking, Tool, ToolChoice, Usage,
};
use crate::conversion::completions::parse_data_url;
use crate::conversion::namespace_tools::NamespaceToolMap;
use crate::conversion::responses::from_messages::{
	error_type, incomplete_reason_to_stop_reason as incomplete_stop_reason,
	translate_stop_reason as stop_reason,
};
use crate::conversion::{GeminiThoughtSignature, ProviderUsage};
use crate::parse::sse::SseJsonEvent;
use crate::types::ResponseType;
use crate::types::responses::typed as responses;
use crate::{
	AIError, OutputMessagePart, StreamingUsageGuard, json as json_util, logged_response_parsing,
	types,
};

fn random_id(prefix: &str) -> String {
	format!("{prefix}_{:016x}", rand::rng().random::<u64>())
}

fn unsupported<T>(reason: &'static str) -> Result<T, AIError> {
	Err(AIError::UnsupportedConversion(strng::new(reason)))
}

/// Also returns the namespaced tools it flattened, so the reply can name them as the client did.
/// Input other than text is dropped.
pub fn parse_request(
	req: &types::responses::Request,
) -> Result<(Request, NamespaceToolMap), AIError> {
	use responses::{
		EasyInputContent, InputContent, InputItem, InputMessage, InputParam, InputRole,
		InputTextContent, Item, MessageItem, OutputMessageContent, Role as ResponsesRole,
		TextResponseFormatConfiguration,
	};

	let mut req = json_util::convert::<_, responses::CreateResponse>(req)
		.map_err(|err| AIError::RequestParsing(crate::InputFormat::Responses, err))?;
	let namespaces = NamespaceToolMap::rewrite_request(&mut req)?;

	let text = |text: InputTextContent| Part::Text {
		text: text.text,
		cache: text.prompt_cache_breakpoint.is_some(),
	};
	let texts = |content: Vec<InputContent>| {
		Content::Parts(
			content
				.into_iter()
				.filter_map(|content| match content {
					InputContent::InputText(t) => Some(text(t)),
					_ => None,
				})
				.collect(),
		)
	};
	let joined = |content: &[InputContent]| {
		content
			.iter()
			.filter_map(|content| match content {
				InputContent::InputText(t) => Some(t.text.as_str()),
				_ => None,
			})
			.collect::<Vec<_>>()
			.join("\n")
	};
	// Consecutive calls share one assistant turn.
	let push_call = |messages: &mut Vec<Message>, call: Part| {
		if let Some(Message {
			role: Role::Assistant,
			content: Content::Parts(parts),
		}) = messages.last_mut()
			&& parts
				.iter()
				.any(|part| matches!(part, Part::ToolCall { .. }))
		{
			parts.push(call);
		} else {
			messages.push(Message {
				role: Role::Assistant,
				content: Content::Parts(vec![call]),
			});
		}
	};
	let tool_result = |id: String, output: String| Message {
		role: Role::User,
		content: Content::Parts(vec![Part::ToolResult {
			id,
			content: Content::Text(output),
			is_error: None,
			cache: false,
		}]),
	};

	let items = match req.input {
		InputParam::Text(text) => vec![InputItem::from(InputMessage {
			content: vec![InputContent::InputText(InputTextContent {
				text,
				prompt_cache_breakpoint: None,
			})],
			role: InputRole::User,
			status: None,
		})],
		InputParam::Items(items) => items,
	};
	let mut messages = Vec::new();
	let mut gemini_signatures = HashMap::new();
	for item in items {
		let message = match item {
			InputItem::EasyMessage(msg) => Message {
				role: match msg.role {
					ResponsesRole::User => Role::User,
					ResponsesRole::Assistant => Role::Assistant,
					ResponsesRole::System | ResponsesRole::Developer => Role::Developer,
				},
				content: match msg.content {
					EasyInputContent::Text(text) => Content::Text(text),
					EasyInputContent::ContentList(parts) => texts(parts),
				},
			},
			InputItem::ItemReference(_)
			| InputItem::Program(_)
			| InputItem::ProgramOutput(_)
			| InputItem::CompactionTrigger(_) => continue,
			InputItem::Item(item) => match item {
				// Gemini signatures ride in reasoning items; they belong to the call they name.
				Item::Reasoning(reasoning) => {
					if let Some(value) = reasoning
						.encrypted_content
						.as_deref()
						.and_then(GeminiThoughtSignature::decode)
					{
						gemini_signatures.insert(value.call_id, value.signature);
					}
					continue;
				},
				Item::Message(MessageItem::Input(msg)) => Message {
					role: match msg.role {
						InputRole::User => Role::User,
						InputRole::System => Role::System,
						InputRole::Developer => Role::Developer,
					},
					content: texts(msg.content),
				},
				Item::Message(MessageItem::Output(msg)) => Message {
					role: Role::Assistant,
					content: Content::Text(
						msg
							.content
							.iter()
							.filter_map(|content| match content {
								OutputMessageContent::OutputText(t) => Some(t.text.as_str()),
								_ => None,
							})
							.collect::<Vec<_>>()
							.join("\n"),
					),
				},
				Item::FunctionCall(call) => {
					push_call(
						&mut messages,
						Part::ToolCall {
							thought_signature: gemini_signatures.remove(&call.call_id),
							id: call.call_id,
							name: call.name,
							arguments: call.arguments,
							cache: false,
						},
					);
					continue;
				},
				Item::FunctionCallOutput(output) => {
					let Some(id) = output.call_id.filter(|id| !id.is_empty()) else {
						continue;
					};
					tool_result(
						id,
						match output.output {
							responses::FunctionCallOutput::Text(text) => text,
							responses::FunctionCallOutput::Content(parts) => joined(&parts),
						},
					)
				},
				Item::CustomToolCall(call) => {
					push_call(
						&mut messages,
						Part::ToolCall {
							arguments: serde_json::to_string(&call.input).unwrap_or_else(|_| "{}".to_string()),
							id: call.id,
							name: call.name,
							thought_signature: None,
							cache: false,
						},
					);
					continue;
				},
				Item::CustomToolCallOutput(output) => {
					let responses::CustomToolCallOutputOutput::Text(text) = output.output else {
						continue;
					};
					tool_result(output.id.unwrap_or_default(), text)
				},
				_ => continue,
			},
		};
		messages.push(message);
	}

	let tools = req.tools.map(|tools| {
		tools
			.into_iter()
			.filter_map(|tool| match tool {
				responses::Tool::Function(tool) => Some(Tool {
					name: tool.name,
					description: tool.description,
					parameters: tool.parameters,
					strict: tool.strict,
					cache: false,
				}),
				_ => None,
			})
			.collect()
	});
	let tool_choice = req.tool_choice.and_then(|choice| {
		use responses::{ToolChoiceFunction, ToolChoiceOptions, ToolChoiceParam};
		match choice {
			ToolChoiceParam::Option(ToolChoiceOptions::Auto) => Some(ToolChoice::Auto),
			ToolChoiceParam::Option(ToolChoiceOptions::Required) => Some(ToolChoice::Required),
			ToolChoiceParam::Option(ToolChoiceOptions::None) => Some(ToolChoice::None),
			ToolChoiceParam::Function(ToolChoiceFunction { name }) => Some(ToolChoice::Tool(name)),
			other => {
				tracing::warn!("dropping unsupported Responses tool choice {other:?}");
				None
			},
		}
	});
	let response_format = req.text.and_then(|text| match text.format {
		TextResponseFormatConfiguration::JsonSchema(schema) => Some(ResponseFormat::JsonSchema {
			name: Some(schema.name),
			description: schema.description,
			schema: schema.schema,
			strict: schema.strict,
		}),
		TextResponseFormatConfiguration::JsonObject => Some(ResponseFormat::JsonObject),
		TextResponseFormatConfiguration::Text => None,
	});
	let request = Request {
		model: req.model,
		instructions: req.instructions.map(|instructions| Message {
			role: Role::Developer,
			content: Content::Text(instructions),
		}),
		messages,
		tools,
		tool_choice,
		parallel_tool_calls: req.parallel_tool_calls,
		max_tokens: req.max_output_tokens.map(|tokens| tokens as usize),
		temperature: req.temperature,
		top_p: req.top_p,
		stop: Vec::new(),
		stream: req.stream.unwrap_or(false),
		metadata: BTreeMap::new(),
		thinking: None,
		effort: req
			.reasoning
			.and_then(|reasoning| reasoning.effort)
			.and_then(|effort| match effort {
				responses::ReasoningEffort::None => None,
				responses::ReasoningEffort::Minimal => Some(Effort::Minimal),
				responses::ReasoningEffort::Low => Some(Effort::Low),
				responses::ReasoningEffort::Medium => Some(Effort::Medium),
				responses::ReasoningEffort::High => Some(Effort::High),
				responses::ReasoningEffort::Xhigh => Some(Effort::Xhigh),
				responses::ReasoningEffort::Max => Some(Effort::Max),
			}),
		response_format,
	};
	Ok((request, namespaces))
}

pub fn render_request(req: Request) -> Result<types::responses::Request, AIError> {
	let model = req.model.unwrap_or_default();
	// Explicit prompt-cache breakpoints are accepted only by GPT 5.6 and newer models.
	let caches = crate::conversion::supports_prompt_cache_breakpoint(&model);
	let mark = |mut value: Value, cache: bool| {
		if caches
			&& cache
			&& let Some(object) = value.as_object_mut()
		{
			object
				.entry("prompt_cache_breakpoint")
				.or_insert_with(|| json!({"mode": "explicit"}));
		}
		value
	};
	let input_text =
		|text: String, cache: bool| mark(json!({"type": "input_text", "text": text}), cache);
	let texts = |content: Content| match content {
		Content::Text(text) => vec![(text, false)],
		Content::Parts(parts) => parts
			.into_iter()
			.filter_map(|part| match part {
				Part::Text { text, cache } => Some((text, cache)),
				_ => None,
			})
			.collect(),
	};

	let mut input = Vec::new();
	let mut rest = Map::new();
	if let Some(instructions) = req.instructions {
		let parts = texts(instructions.content);
		// Only a message can carry cache marks, so marked instructions go in as one.
		if caches && parts.iter().any(|(_, cache)| *cache) {
			let parts = parts
				.into_iter()
				.map(|(text, cache)| input_text(text, cache))
				.collect();
			push_message(&mut input, "system", parts);
		} else {
			let text = parts
				.into_iter()
				.map(|(text, _)| text)
				.collect::<Vec<_>>()
				.join("\n");
			if !text.is_empty() {
				rest.insert("instructions".to_string(), Value::String(text));
			}
		}
	}

	let effort = req.effort.or(match req.thinking {
		Some(Thinking::Enabled { budget_tokens }) => Some(super::completions::messages_effort(
			crate::types::anthropic_effort_for_thinking_budget(budget_tokens),
		)),
		_ => None,
	});
	let reasoning = match req.thinking {
		Some(Thinking::Disabled) => false,
		None => effort.is_some(),
		_ => true,
	};
	if reasoning && crate::conversion::supports_reasoning_effort(&model) {
		let reasoning = responses::Reasoning {
			context: None,
			effort: Some(match effort {
				Some(Effort::Minimal) => responses::ReasoningEffort::Minimal,
				Some(Effort::Low) => responses::ReasoningEffort::Low,
				Some(Effort::Medium) => responses::ReasoningEffort::Medium,
				Some(Effort::Xhigh) => responses::ReasoningEffort::Xhigh,
				Some(Effort::Max) => responses::ReasoningEffort::Max,
				Some(Effort::High) | None => responses::ReasoningEffort::High,
			}),
			mode: None,
			summary: None,
		};
		rest.insert(
			"reasoning".to_string(),
			serde_json::to_value(reasoning).map_err(AIError::RequestMarshal)?,
		);
	}
	if let Some(format) = req.response_format {
		let format = match format {
			ResponseFormat::JsonSchema {
				name,
				schema,
				strict,
				..
			} => json!({
				"type": "json_schema",
				"name": name.unwrap_or_else(|| "structured_output".to_string()),
				"schema": schema,
				// Responses defaults to strict schemas, making optional properties required.
				"strict": strict.unwrap_or(false),
			}),
			ResponseFormat::JsonObject => json!({"type": "json_object"}),
		};
		rest.insert("text".to_string(), json!({ "format": format }));
	}
	if !req.metadata.is_empty() {
		rest.insert(
			"metadata".to_string(),
			serde_json::to_value(req.metadata).map_err(AIError::RequestMarshal)?,
		);
	}
	let tools: Vec<_> = req
		.tools
		.into_iter()
		.flatten()
		.map(|tool| {
			let mut value = Map::new();
			value.insert("type".to_string(), Value::String("function".to_string()));
			value.insert("name".to_string(), Value::String(tool.name));
			if let Some(description) = tool.description {
				value.insert("description".to_string(), Value::String(description));
			}
			value.insert(
				"parameters".to_string(),
				tool.parameters.unwrap_or_default(),
			);
			// Responses defaults to strict schemas, making optional properties required.
			value.insert(
				"strict".to_string(),
				Value::Bool(tool.strict.unwrap_or(false)),
			);
			Value::Object(value)
		})
		.collect();
	if !tools.is_empty() {
		rest.insert("tools".to_string(), Value::Array(tools));
	}
	let tool_choice = req.tool_choice.and_then(|choice| match choice {
		ToolChoice::Auto => Some(json!("auto")),
		ToolChoice::Required => Some(json!("required")),
		ToolChoice::None => Some(json!("none")),
		ToolChoice::Tool(name) => Some(json!({"type": "function", "name": name})),
		ToolChoice::Other => None,
	});
	if let Some(choice) = tool_choice {
		rest.insert("tool_choice".to_string(), choice);
		if let Some(parallel) = req.parallel_tool_calls {
			rest.insert("parallel_tool_calls".to_string(), Value::Bool(parallel));
		}
	}

	for msg in req.messages {
		match msg.role {
			Role::System | Role::Developer => {
				let parts = texts(msg.content)
					.into_iter()
					.map(|(text, cache)| input_text(text, cache))
					.collect();
				let role = if msg.role == Role::System {
					"system"
				} else {
					"developer"
				};
				push_message(&mut input, role, parts);
			},
			Role::User => {
				let mut parts = Vec::new();
				let content = match msg.content {
					Content::Text(text) => vec![Part::Text { text, cache: false }],
					Content::Parts(parts) => parts,
				};
				for part in content {
					match part {
						Part::Text { text, cache } => parts.push(input_text(text, cache)),
						Part::Image { source, cache } => parts.push(mark(image(source)?, cache)),
						Part::Document {
							source,
							title,
							cache,
						} => parts.push(mark(document(source, title.as_deref())?, cache)),
						// A tool result is its own item, between the turn's other content.
						Part::ToolResult {
							id,
							content,
							is_error,
							cache,
						} => {
							push_message(&mut input, "user", std::mem::take(&mut parts));
							input.push(json!({
								"type": "function_call_output",
								"call_id": id,
								"output": tool_output(content, cache && caches, &mark)?,
								"status": if is_error.unwrap_or_default() { "incomplete" } else { "completed" },
							}));
						},
						_ => {},
					}
				}
				push_message(&mut input, "user", parts);
			},
			Role::Assistant => {
				let mut parts = Vec::new();
				let content = match msg.content {
					Content::Text(text) => vec![Part::Text { text, cache: false }],
					Content::Parts(parts) => parts,
				};
				for part in content {
					match part {
						Part::Text { text, cache } => parts.push(mark(
							json!({"type": "output_text", "text": text, "annotations": []}),
							cache,
						)),
						// Messages keeps the call's id, not the optional Responses item id.
						Part::ToolCall {
							id,
							name,
							arguments,
							..
						} => {
							push_output(&mut input, std::mem::take(&mut parts));
							input.push(json!({
								"type": "function_call",
								"call_id": id,
								"name": name,
								"arguments": arguments,
								"status": "completed",
							}));
						},
						_ => {},
					}
				}
				push_output(&mut input, parts);
			},
		}
	}

	let max_output_tokens = req
		.max_tokens
		.map(|tokens| {
			u32::try_from(tokens).map_err(|_| {
				AIError::UnsupportedConversion(strng::literal!(
					"messages max_tokens exceeds responses max_output_tokens range"
				))
			})
		})
		.transpose()?;
	Ok(types::responses::Request {
		input: types::responses::RequestInput::Items(
			input
				.into_iter()
				.map(types::responses::RawInputItem::from_value)
				.collect(),
		),
		model: Some(model),
		moderation: None,
		max_output_tokens,
		temperature: req.temperature,
		top_p: req.top_p,
		stream: Some(req.stream),
		instructions: None,
		vendor_extensions: None,
		reasoning: None,
		rest: Value::Object(rest),
	})
}

fn push_message(input: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
	if !parts.is_empty() {
		input.push(json!({"type": "message", "role": role, "content": parts}));
	}
}

fn push_output(input: &mut Vec<Value>, parts: Vec<Value>) {
	if !parts.is_empty() {
		input.push(json!({
			"type": "message",
			"id": random_id("msg"),
			"role": "assistant",
			"content": parts,
			"status": "completed",
		}));
	}
}

fn image(source: Source) -> Result<Value, AIError> {
	match source {
		Source::Url(url) => Ok(json!({"type": "input_image", "image_url": url, "detail": "auto"})),
		Source::FileId(id) => Ok(json!({"type": "input_image", "file_id": id, "detail": "auto"})),
		Source::Text(_) => unsupported("messages image source cannot be represented by responses"),
		Source::Invalid(reason) => unsupported(reason),
	}
}

/// A source Responses can't carry is refused, never dropped: a model that answers without the
/// document it was given is worse than a refused request.
fn document(source: Source, title: Option<&str>) -> Result<Value, AIError> {
	match source {
		Source::Url(url) => match parse_data_url(&url) {
			Some((media_type, _)) => {
				// OpenAI requires a filename alongside inline file data.
				let filename = match (title, media_type) {
					(Some(title), _) => title.to_string(),
					(None, "application/pdf") => "document.pdf".to_string(),
					(None, _) => "document".to_string(),
				};
				Ok(json!({"type": "input_file", "filename": filename, "file_data": url}))
			},
			None => Ok(json!({"type": "input_file", "file_url": url})),
		},
		Source::FileId(id) => Ok(json!({"type": "input_file", "file_id": id})),
		Source::Text(text) => Ok(json!({"type": "input_text", "text": text})),
		Source::Invalid(reason) => unsupported(reason),
	}
}

/// A tool result's output: plain text where it can be, else a list of parts.
fn tool_output(
	content: Content,
	cache: bool,
	mark: &impl Fn(Value, bool) -> Value,
) -> Result<Value, AIError> {
	let parts = match content {
		Content::Text(text) if cache => vec![Part::Text { text, cache: true }],
		Content::Text(text) => return Ok(Value::String(text)),
		Content::Parts(parts) => parts,
	};
	let mut texts = Vec::new();
	let mut values = Vec::new();
	let mut array = cache;
	for part in parts {
		match part {
			Part::Text { text, cache } => {
				values.push(mark(json!({"type": "input_text", "text": &text}), cache));
				texts.push(text);
			},
			Part::Image { source, cache } => {
				values.push(mark(image(source)?, cache));
				array = true;
			},
			Part::Document {
				source,
				title,
				cache,
			} => {
				values.push(mark(document(source, title.as_deref())?, cache));
				array = true;
			},
			_ => {},
		}
	}
	if cache {
		match values.last_mut() {
			Some(last) => *last = mark(std::mem::take(last), true),
			None => values.push(mark(json!({"type": "input_text", "text": ""}), true)),
		}
	}
	if array
		|| values
			.iter()
			.any(|value| value.get("prompt_cache_breakpoint").is_some())
	{
		Ok(Value::Array(values))
	} else {
		Ok(Value::String(texts.join("\n")))
	}
}

fn usage(u: &responses::ResponseUsage) -> Usage {
	Usage {
		input_tokens: u.input_tokens as u64,
		output_tokens: u.output_tokens as u64,
		total_tokens: Some(u.total_tokens as u64),
		reasoning_tokens: Some(u.output_tokens_details.reasoning_tokens as u64),
		cache_read_tokens: Some(u.input_tokens_details.cached_tokens as u64).filter(|t| *t > 0),
		cache_write_tokens: u
			.input_tokens_details
			.cache_write_tokens
			.map(|t| t as u64)
			.filter(|t| *t > 0),
		input_audio_tokens: None,
		output_audio_tokens: None,
	}
}

pub fn parse_response(bytes: &Bytes) -> Result<Response, AIError> {
	let resp =
		serde_json::from_slice::<responses::Response>(bytes).map_err(logged_response_parsing(bytes))?;
	let provider_usage = resp.usage.as_ref().map(|u| ProviderUsage {
		input_tokens: u.input_tokens as u64,
		total_tokens: u.total_tokens as u64,
		reasoning_tokens: Some(u.output_tokens_details.reasoning_tokens as u64),
		cache_creation_12h_input_tokens: u
			.input_tokens_details
			.cache_write_tokens_details
			.as_ref()
			.and_then(|d| d.ttl_12h_tokens)
			.map(|t| t as u64),
		..Default::default()
	});
	if resp.error.is_some() || matches!(resp.status, responses::Status::Failed) {
		return unsupported("responses failed body cannot be represented by messages");
	}
	if matches!(
		resp.status,
		responses::Status::InProgress | responses::Status::Queued | responses::Status::Cancelled
	) {
		return unsupported("non-final responses body cannot be represented by messages");
	}

	let mut parts = Vec::new();
	let mut saw_tool_call = false;
	let mut saw_refusal = false;
	for item in resp.output {
		match item {
			responses::OutputItem::Message(message) => {
				if matches!(message.status, responses::OutputStatus::InProgress) {
					return unsupported("in-progress responses message cannot be represented by messages");
				}
				for part in message.content {
					match part {
						responses::OutputMessageContent::OutputText(text) => {
							let citations = text
								.annotations
								.into_iter()
								.filter_map(|annotation| {
									let responses::Annotation::UrlCitation(citation) = annotation else {
										return None;
									};
									let citation = serde_json::to_value(citation).ok()?;
									Some(Citation {
										url: citation["url"].clone(),
										title: citation["title"].clone(),
									})
								})
								.collect();
							parts.push(ReplyPart::Text {
								text: text.text,
								citations,
							});
						},
						responses::OutputMessageContent::Refusal(refusal) => {
							saw_refusal = true;
							parts.push(ReplyPart::Refusal(refusal.refusal));
						},
					}
				}
			},
			responses::OutputItem::FunctionCall(call) => {
				if call.namespace.is_some() || call.caller.is_some() {
					return unsupported(
						"responses namespaced/caller function calls cannot be represented by messages",
					);
				}
				if matches!(call.status, Some(responses::OutputStatus::InProgress)) {
					return unsupported(
						"in-progress responses function call cannot be represented by messages",
					);
				}
				saw_tool_call = true;
				parts.push(ReplyPart::ToolCall {
					id: call.call_id,
					name: call.name,
					arguments: call.arguments,
					thought_signature: None,
				});
			},
			responses::OutputItem::Reasoning(_) => {
				tracing::warn!("skipping a Responses reasoning item no other format carries");
			},
			_ => return unsupported("responses output item cannot be represented by messages"),
		}
	}
	let stop_reason = stop_reason(
		&resp.status,
		resp.incomplete_details.as_ref(),
		saw_tool_call,
		saw_refusal,
	)?;
	Ok(Response {
		id: resp.id,
		model: resp.model,
		parts,
		stop_reason: Some(stop_reason),
		stop_sequence: None,
		usage: resp.usage.as_ref().map(usage),
		service_tier: resp
			.service_tier
			.as_ref()
			.and_then(types::serialize_str)
			.map(|tier| tier.to_string()),
		provider_usage,
	})
}

/// The reply as a Responses body. Responses names the model the client asked for.
pub fn render_response(
	resp: Response,
	model: &str,
	namespaces: Option<&NamespaceToolMap>,
) -> Result<Box<dyn ResponseType>, AIError> {
	let builder = types::responses::ResponseBuilder::new(random_id("resp"), model.to_string());
	let mut output = Vec::new();
	let mut text = Vec::new();
	let mut tool_calls = Vec::new();
	for part in resp.parts {
		match part {
			ReplyPart::Reasoning { text, .. } if !text.is_empty() => {
				output.push(reasoning_item(text, responses::OutputStatus::Completed))
			},
			ReplyPart::Reasoning { .. } => {},
			ReplyPart::Text { text: t, .. } => text.push(responses::OutputMessageContent::OutputText(
				responses::OutputTextContent {
					annotations: vec![],
					logprobs: None,
					text: t,
				},
			)),
			ReplyPart::Refusal(refusal) => text.push(responses::OutputMessageContent::Refusal(
				async_openai::types::responses::RefusalContent { refusal },
			)),
			ReplyPart::ToolCall {
				id,
				name,
				arguments,
				thought_signature,
			} => {
				// A Gemini signature rides in a reasoning item ahead of its call.
				if let Some(signature) = thought_signature {
					tool_calls.push(responses::OutputItem::Reasoning(responses::ReasoningItem {
						id: Some(random_id("rs")),
						summary: vec![],
						content: None,
						encrypted_content: Some(GeminiThoughtSignature::encode(&id, &signature)),
						status: Some(responses::OutputStatus::Completed),
					}));
				}
				tool_calls.push(function_call(
					id,
					name,
					arguments,
					responses::OutputStatus::Completed,
				));
			},
		}
	}
	if !text.is_empty() {
		output.push(responses::OutputItem::Message(responses::OutputMessage {
			id: random_id("msg"),
			role: responses::AssistantRole::Assistant,
			phase: None,
			content: text,
			status: responses::OutputStatus::Completed,
		}));
	}
	output.extend(tool_calls);
	let (status, error, incomplete) = status(resp.stop_reason);
	let mut response = builder.response(status, resp.usage.map(response_usage), error, incomplete);
	response.output = output;
	if let Some(namespaces) = namespaces {
		namespaces.restore_response(&mut response);
	}
	let response = json_util::convert::<_, types::responses::Response>(&response)
		.map_err(AIError::ResponseParsing)?;
	Ok(Box::new(response))
}

fn reasoning_item(text: String, status: responses::OutputStatus) -> responses::OutputItem {
	responses::OutputItem::Reasoning(responses::ReasoningItem {
		id: Some(random_id("rs")),
		summary: vec![],
		content: Some(vec![responses::ReasoningItemContent::ReasoningText(
			responses::ReasoningTextContent { text },
		)]),
		encrypted_content: None,
		status: Some(status),
	})
}

fn function_call(
	id: String,
	name: String,
	arguments: String,
	status: responses::OutputStatus,
) -> responses::OutputItem {
	responses::OutputItem::FunctionCall(responses::FunctionToolCall {
		arguments,
		call_id: id.clone(),
		name,
		caller: None,
		id: Some(id),
		status: Some(status),
		namespace: None,
		r#async: None,
	})
}

/// How a reply ended, as Responses says it.
fn status(
	stop_reason: Option<StopReason>,
) -> (
	responses::Status,
	Option<responses::ResponseError>,
	Option<responses::IncompleteDetails>,
) {
	match stop_reason {
		Some(StopReason::MaxTokens | StopReason::ModelContextWindowExceeded) => (
			responses::Status::Incomplete,
			None,
			Some(responses::IncompleteDetails {
				reason: "max_tokens".to_string(),
			}),
		),
		Some(StopReason::Refusal) => (
			responses::Status::Failed,
			Some(responses::ResponseError {
				code: responses::ResponseErrorCode::Other("content_filter".to_string()),
				message: "Content filtered".to_string(),
				misalignment: None,
			}),
			None,
		),
		_ => (responses::Status::Completed, None, None),
	}
}

fn response_usage(usage: Usage) -> responses::ResponseUsage {
	responses::ResponseUsage {
		input_tokens: usage.input_tokens as u32,
		output_tokens: usage.output_tokens as u32,
		total_tokens: usage
			.total_tokens
			.unwrap_or(usage.input_tokens.saturating_add(usage.output_tokens)) as u32,
		input_tokens_details: responses::InputTokenDetails {
			cached_tokens: usage.cache_read_tokens.unwrap_or_default() as u32,
			cache_write_tokens: usage.cache_write_tokens.map(|tokens| tokens as i64),
			cache_write_tokens_details: None,
		},
		output_tokens_details: responses::OutputTokenDetails {
			reasoning_tokens: usage.reasoning_tokens.unwrap_or_default() as u32,
		},
	}
}

/// Reads a Responses provider stream into events, recording its usage, timing and content to
/// `log`. A failed reply stops the stream.
pub fn parse_stream(
	log: StreamingUsageGuard,
	log_content: crate::LogContentFields,
) -> impl FnMut(SseJsonEvent<responses::ResponseStreamEvent>) -> Vec<StreamEvent> + Send + 'static {
	#[derive(Default)]
	struct State {
		started: bool,
		ended: bool,
		failed: bool,
		last_token_at: Option<Instant>,
		response_id: Option<String>,
		model: Option<String>,
		/// Text parts by output and content index.
		texts: HashMap<(u32, u32), usize>,
		refusals: HashSet<(u32, u32)>,
		/// Tool calls by output index, and whether their arguments were sent.
		tools: HashMap<u32, bool>,
		usage: Option<responses::ResponseUsage>,
		stop_reason: Option<StopReason>,
		saw_tool_call: bool,
		saw_refusal: bool,
		completion: Option<String>,
		tool_calls: Option<BTreeMap<u32, OutputMessagePart>>,
	}
	impl State {
		fn start(&mut self, out: &mut Vec<StreamEvent>, log: &StreamingUsageGuard) {
			if std::mem::replace(&mut self.started, true) {
				return;
			}
			let model = self.model.clone().unwrap_or_default();
			if !model.is_empty() {
				log.update(|r| r.response.provider_model = Some(strng::new(&model)));
			}
			out.push(StreamEvent::Start {
				id: self.response_id.clone().unwrap_or_else(|| random_id("msg")),
				model,
				service_tier: None,
			});
		}

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

		fn text(&mut self, key: (u32, u32)) -> usize {
			let next = self.texts.len();
			*self.texts.entry(key).or_insert(next)
		}

		/// Another piece of tool call `index`, which starts with a fallback id and name.
		fn tool(
			&mut self,
			out: &mut Vec<StreamEvent>,
			index: u32,
			id: Option<String>,
			name: Option<String>,
			arguments: String,
		) {
			self.saw_tool_call = true;
			let (id, name) = if self.tools.contains_key(&index) {
				(None, None)
			} else {
				self.tools.insert(index, false);
				(
					Some(id.unwrap_or_else(|| format!("call_{index}"))),
					Some(name.unwrap_or_default()),
				)
			};
			out.push(StreamEvent::ToolCall {
				index: index as usize,
				id,
				name,
				arguments,
			});
		}

		fn metadata(&mut self, log: &StreamingUsageGuard, response: &responses::Response) {
			self.response_id = Some(response.id.clone());
			self.model = Some(response.model.clone());
			let service_tier = response
				.service_tier
				.as_ref()
				.and_then(types::serialize_str);
			log.update(|r| {
				r.response.provider_model = Some(strng::new(&response.model));
				r.response.service_tier = service_tier.clone();
			});
			if let Some(usage) = &response.usage {
				self.usage = Some(usage.clone());
				log.update(|r| {
					r.response.input_tokens = Some(usage.input_tokens as u64);
					r.response.output_tokens = Some(usage.output_tokens as u64);
					r.response.total_tokens = Some(usage.total_tokens as u64);
					r.response.cached_input_tokens = Some(usage.input_tokens_details.cached_tokens as u64);
					r.response.cache_creation_input_tokens = usage
						.input_tokens_details
						.cache_write_tokens
						.map(|tokens| tokens as u64);
					r.response.cache_creation_12h_input_tokens = usage
						.input_tokens_details
						.cache_write_tokens_details
						.as_ref()
						.and_then(|d| d.ttl_12h_tokens)
						.map(|t| t as u64);
					r.response.reasoning_tokens = Some(usage.output_tokens_details.reasoning_tokens as u64);
					if let Some(service_tier) = &service_tier {
						r.response.service_tier = Some(service_tier.clone());
					}
				});
			}
		}

		/// Final events repeat the whole refusal, which goes out only when no piece of it did.
		fn refusal(
			&mut self,
			out: &mut Vec<StreamEvent>,
			log: &StreamingUsageGuard,
			key: (u32, u32),
			text: String,
		) {
			self.saw_refusal = true;
			self.stop_reason = Some(StopReason::Refusal);
			let index = self.text(key);
			if !text.is_empty() && self.refusals.insert(key) {
				self.start(out, log);
				self.token(log);
				if let Some(completion) = self.completion.as_mut() {
					completion.push_str(&text);
				}
				out.push(StreamEvent::Text { index, text });
			}
			out.push(StreamEvent::TextEnd { index });
		}

		fn end(&mut self, out: &mut Vec<StreamEvent>, log: &StreamingUsageGuard) {
			if std::mem::replace(&mut self.ended, true) {
				return;
			}
			self.start(out, log);
			let stop_reason = self.stop_reason.take().unwrap_or(if self.saw_tool_call {
				StopReason::ToolUse
			} else {
				StopReason::EndTurn
			});
			out.push(StreamEvent::End {
				stop_reason: Some(stop_reason),
				stop_sequence: None,
				usage: self.usage.take().as_ref().map(usage),
			});
			let finish_reason = types::serialize_str(&stop_reason);
			self.log_content(log, finish_reason);
		}

		fn log_content(&mut self, log: &StreamingUsageGuard, finish_reason: Option<strng::Strng>) {
			let completion = self.completion.take();
			let tool_calls = take_output_messages(&mut self.tool_calls, finish_reason);
			let mut completion = completion;
			let mut tool_calls = Some(tool_calls);
			log.update(|r| {
				if let Some(completion) = completion.take() {
					r.response.completion = Some(vec![completion]);
				}
				r.response.output_messages = tool_calls.take().flatten();
			});
		}
	}

	let mut state = State {
		completion: log_content.completion.then(String::new),
		tool_calls: log_content.tool_calls.then(BTreeMap::new),
		..Default::default()
	};
	move |event| {
		use responses::ResponseStreamEvent as E;
		let mut out = Vec::new();
		if state.failed {
			return out;
		}
		match event {
			SseJsonEvent::Eof | SseJsonEvent::Error => return out,
			SseJsonEvent::Done => state.end(&mut out, &log),
			SseJsonEvent::Data(Err(e)) => {
				tracing::warn!("failed to parse a Responses stream event: {e}");
			},
			SseJsonEvent::Data(Ok(event)) => match event {
				E::ResponseCreated(created) => {
					state.metadata(&log, &created.response);
					state.start(&mut out, &log);
				},
				E::ResponseOutputItemAdded(added) => {
					state.start(&mut out, &log);
					if let responses::OutputItem::FunctionCall(call) = added.item {
						state.tool(
							&mut out,
							added.output_index,
							Some(call.call_id),
							Some(call.name),
							String::new(),
						);
						state.token(&log);
					}
				},
				E::ResponseContentPartAdded(added) => {
					state.start(&mut out, &log);
					let index = state.text((added.output_index, added.content_index));
					match added.part {
						responses::OutputContent::OutputText(_) => out.push(StreamEvent::TextStart { index }),
						responses::OutputContent::Refusal(_) => {
							state.stop_reason = Some(StopReason::Refusal);
							out.push(StreamEvent::TextStart { index });
						},
						responses::OutputContent::ReasoningText(_) => {
							tracing::warn!("skipping streamed Responses reasoning no other format carries");
						},
					}
				},
				E::ResponseOutputTextDelta(delta) => {
					state.start(&mut out, &log);
					let index = state.text((delta.output_index, delta.content_index));
					state.token(&log);
					if let Some(completion) = state.completion.as_mut() {
						completion.push_str(&delta.delta);
					}
					out.push(StreamEvent::Text {
						index,
						text: delta.delta,
					});
				},
				E::ResponseFunctionCallArgumentsDelta(delta) => {
					state.start(&mut out, &log);
					state.token(&log);
					state.tool(
						&mut out,
						delta.output_index,
						Some(delta.item_id),
						None,
						delta.delta,
					);
					state.tools.insert(delta.output_index, true);
				},
				E::ResponseFunctionCallArgumentsDone(done) => {
					state.start(&mut out, &log);
					let send = !state
						.tools
						.get(&done.output_index)
						.copied()
						.unwrap_or_default()
						&& !done.arguments.is_empty();
					if send {
						state.token(&log);
					}
					state.tool(
						&mut out,
						done.output_index,
						Some(done.item_id),
						done.name,
						if send { done.arguments } else { String::new() },
					);
					if send {
						state.tools.insert(done.output_index, true);
					}
				},
				E::ResponseRefusalDelta(delta) => {
					state.saw_refusal = true;
					state.stop_reason = Some(StopReason::Refusal);
					let key = (delta.output_index, delta.content_index);
					let index = state.text(key);
					if delta.delta.is_empty() {
						out.push(StreamEvent::TextStart { index });
					} else {
						state.refusals.insert(key);
						state.token(&log);
						if let Some(completion) = state.completion.as_mut() {
							completion.push_str(&delta.delta);
						}
						out.push(StreamEvent::Text {
							index,
							text: delta.delta,
						});
					}
				},
				E::ResponseRefusalDone(done) => state.refusal(
					&mut out,
					&log,
					(done.output_index, done.content_index),
					done.refusal,
				),
				E::ResponseContentPartDone(done) => {
					let key = (done.output_index, done.content_index);
					match done.part {
						responses::OutputContent::Refusal(refusal) => {
							state.refusal(&mut out, &log, key, refusal.refusal)
						},
						_ => {
							let index = state.text(key);
							out.push(StreamEvent::TextEnd { index });
						},
					}
				},
				E::ResponseOutputItemDone(done) => match done.item {
					responses::OutputItem::FunctionCall(call) => {
						let send = !state
							.tools
							.get(&done.output_index)
							.copied()
							.unwrap_or_default()
							&& !call.arguments.is_empty();
						state.tool(
							&mut out,
							done.output_index,
							Some(call.call_id.clone()),
							Some(call.name.clone()),
							if send {
								call.arguments.clone()
							} else {
								String::new()
							},
						);
						if send {
							state.tools.insert(done.output_index, true);
						}
						if let Some(tool_calls) = state.tool_calls.as_mut() {
							tool_calls.insert(
								done.output_index,
								OutputMessagePart::ToolCall {
									id: strng::new(&call.call_id),
									name: strng::new(&call.name),
									arguments: serde_json::from_str(&call.arguments)
										.unwrap_or_else(|_| Value::Object(Map::new())),
								},
							);
						}
						out.push(StreamEvent::ToolCallEnd {
							index: done.output_index as usize,
						});
					},
					responses::OutputItem::Message(_) => {
						let mut open: Vec<_> = state
							.texts
							.iter()
							.filter(|((output, _), _)| *output == done.output_index)
							.map(|(_, index)| *index)
							.collect();
						open.sort();
						out.extend(open.into_iter().map(|index| StreamEvent::TextEnd { index }));
					},
					_ => {
						tracing::warn!("skipping a streamed Responses output item no other format carries");
					},
				},
				E::ResponseCompleted(completed) => {
					state.metadata(&log, &completed.response);
					state.stop_reason = Some(if state.saw_tool_call {
						StopReason::ToolUse
					} else if state.saw_refusal {
						StopReason::Refusal
					} else {
						StopReason::EndTurn
					});
					state.end(&mut out, &log);
				},
				E::ResponseIncomplete(incomplete) => {
					state.metadata(&log, &incomplete.response);
					state.stop_reason = incomplete
						.response
						.incomplete_details
						.as_ref()
						.and_then(|details| incomplete_stop_reason(&details.reason).ok());
					state.end(&mut out, &log);
				},
				E::ResponseFailed(failed) => {
					state.metadata(&log, &failed.response);
					tracing::warn!("Responses stream failed");
					state.failed = true;
					let error = failed.response.error;
					out.push(StreamEvent::Error {
						kind: error_type(
							error
								.as_ref()
								.and_then(|e| serde_json::to_value(&e.code).ok())
								.as_ref()
								.and_then(Value::as_str),
						)
						.to_string(),
						message: error
							.map(|error| error.message)
							.unwrap_or_else(|| "responses stream failed".to_string()),
					});
				},
				E::ResponseError(error) => {
					tracing::warn!("Responses stream error: {}", error.message);
					state.failed = true;
					out.push(StreamEvent::Error {
						kind: error_type(error.code.as_deref()).to_string(),
						message: error.message,
					});
				},
				E::ResponseInProgress(_)
				| E::ResponseOutputTextDone(_)
				| E::ResponseReasoningTextDelta(_)
				| E::ResponseReasoningTextDone(_) => {},
			},
		}
		// A failed reply keeps what it produced for the log, without a finish reason.
		if state.failed {
			state.log_content(&log, None);
		}
		out
	}
}

fn take_output_messages(
	tool_calls: &mut Option<BTreeMap<u32, OutputMessagePart>>,
	finish_reason: Option<strng::Strng>,
) -> Option<Vec<crate::OutputMessage>> {
	let content: Vec<_> = std::mem::take(tool_calls.as_mut()?).into_values().collect();
	(!content.is_empty()).then(|| {
		vec![crate::OutputMessage {
			role: strng::literal!("assistant"),
			content,
			finish_reason,
		}]
	})
}

/// Writes events as a Responses client stream. Responses repeats each item whole when it is
/// done, so the text, reasoning and arguments are kept until then.
pub fn render_stream<I: DeserializeOwned + 'static>(
	body: Body,
	buffer_limit: usize,
	mut parse: impl FnMut(SseJsonEvent<I>) -> Vec<StreamEvent> + Send + 'static,
	namespaces: Option<Arc<NamespaceToolMap>>,
) -> Body {
	use responses::ResponseStreamEvent as E;

	struct ToolCall {
		id: String,
		name: String,
		arguments: String,
		output_index: u32,
	}
	struct Items {
		sequence: u64,
		response_id: String,
		message_id: String,
		builder: Option<types::responses::ResponseBuilder>,
		/// Output indexes follow arrival order. The message opens lazily so reasoning, which
		/// streams first, takes index 0 as it does in Responses.
		next_output: u32,
		message_output: Option<u32>,
		/// The reasoning item's id, output index and text.
		reasoning: Option<(String, u32, String)>,
		tool_calls: HashMap<usize, ToolCall>,
		sent_content_part: bool,
		text: String,
		ended: bool,
	}
	impl Items {
		fn push(&mut self, out: &mut Vec<(&'static str, E)>, event: impl FnOnce(u64) -> E) {
			self.sequence += 1;
			out.push(("event", event(self.sequence)));
		}

		fn open_message(&mut self, out: &mut Vec<(&'static str, E)>) -> u32 {
			if let Some(index) = self.message_output {
				return index;
			}
			let output_index = self.next_output;
			self.next_output += 1;
			self.message_output = Some(output_index);
			let id = self.message_id.clone();
			self.push(out, |sequence_number| {
				E::ResponseOutputItemAdded(responses::ResponseOutputItemAddedEvent {
					sequence_number,
					output_index,
					item: responses::OutputItem::Message(responses::OutputMessage {
						content: Vec::new(),
						id,
						role: responses::AssistantRole::Assistant,
						phase: None,
						status: responses::OutputStatus::InProgress,
					}),
				})
			});
			output_index
		}

		fn write(&mut self, event: StreamEvent, out: &mut Vec<(&'static str, E)>) {
			match event {
				StreamEvent::Start { model, .. } => {
					let builder = self.builder.insert(types::responses::ResponseBuilder::new(
						self.response_id.clone(),
						model,
					));
					let created = builder.created_event(self.sequence + 1);
					let in_progress = builder.response(responses::Status::InProgress, None, None, None);
					self.push(out, |_| created);
					// `response.in_progress` follows `response.created`, as in Responses.
					self.push(out, |sequence_number| {
						E::ResponseInProgress(responses::ResponseInProgressEvent {
							sequence_number,
							response: in_progress,
						})
					});
				},
				StreamEvent::Reasoning(delta) => {
					if self.reasoning.is_none() {
						let id = random_id("rs");
						let output_index = self.next_output;
						self.next_output += 1;
						let item_id = id.clone();
						self.push(out, |sequence_number| {
							E::ResponseOutputItemAdded(responses::ResponseOutputItemAddedEvent {
								sequence_number,
								output_index,
								item: responses::OutputItem::Reasoning(responses::ReasoningItem {
									id: Some(item_id),
									summary: vec![],
									content: None,
									encrypted_content: None,
									status: Some(responses::OutputStatus::InProgress),
								}),
							})
						});
						self.reasoning = Some((id, output_index, String::new()));
					}
					let (item_id, output_index, text) = self.reasoning.as_mut().expect("opened above");
					text.push_str(&delta);
					let (item_id, output_index) = (item_id.clone(), *output_index);
					self.push(out, |sequence_number| {
						E::ResponseReasoningTextDelta(responses::ResponseReasoningTextDeltaEvent {
							sequence_number,
							item_id,
							output_index,
							content_index: 0,
							delta,
						})
					});
				},
				StreamEvent::Text { text, .. } => {
					self.text.push_str(&text);
					let output_index = self.open_message(out);
					let item_id = self.message_id.clone();
					if !std::mem::replace(&mut self.sent_content_part, true) {
						let item_id = item_id.clone();
						self.push(out, |sequence_number| {
							E::ResponseContentPartAdded(responses::ResponseContentPartAddedEvent {
								sequence_number,
								item_id,
								output_index,
								content_index: 0,
								part: responses::OutputContent::OutputText(responses::OutputTextContent {
									text: String::new(),
									annotations: Vec::new(),
									logprobs: None,
								}),
							})
						});
					}
					self.push(out, |sequence_number| {
						E::ResponseOutputTextDelta(responses::ResponseTextDeltaEvent {
							sequence_number,
							item_id,
							output_index,
							content_index: 0,
							delta: text,
							logprobs: None,
						})
					});
				},
				StreamEvent::ToolCall {
					index,
					id,
					name,
					arguments,
				} => {
					// The message keeps its place ahead of the tool calls.
					self.open_message(out);
					let new = !self.tool_calls.contains_key(&index);
					if new {
						let output_index = self.next_output;
						self.next_output += 1;
						self.tool_calls.insert(
							index,
							ToolCall {
								id: id.unwrap_or_else(|| random_id("call")),
								name: String::new(),
								arguments: String::new(),
								output_index,
							},
						);
					}
					let call = self.tool_calls.get_mut(&index).expect("inserted above");
					if let Some(name) = name {
						call.name = name;
					}
					call.arguments.push_str(&arguments);
					let (id, name, output_index) = (call.id.clone(), call.name.clone(), call.output_index);
					if new {
						let id = id.clone();
						self.push(out, |sequence_number| {
							E::ResponseOutputItemAdded(responses::ResponseOutputItemAddedEvent {
								sequence_number,
								output_index,
								item: function_call(id, name, String::new(), responses::OutputStatus::InProgress),
							})
						});
					}
					if !arguments.is_empty() {
						self.push(out, |sequence_number| {
							E::ResponseFunctionCallArgumentsDelta(
								responses::ResponseFunctionCallArgumentsDeltaEvent {
									sequence_number,
									item_id: id,
									output_index,
									delta: arguments,
								},
							)
						});
					}
				},
				StreamEvent::End {
					stop_reason, usage, ..
				} => self.end(stop_reason, usage, out),
				// Responses repeats each part whole when its item is done, at the end.
				StreamEvent::TextStart { .. }
				| StreamEvent::TextEnd { .. }
				| StreamEvent::Signature(_)
				| StreamEvent::ToolCallEnd { .. }
				| StreamEvent::Error { .. } => {},
			}
		}

		fn end(
			&mut self,
			stop_reason: Option<StopReason>,
			usage: Option<Usage>,
			out: &mut Vec<(&'static str, E)>,
		) {
			if std::mem::replace(&mut self.ended, true) {
				return;
			}
			let response_id = self.response_id.clone();
			self
				.builder
				.get_or_insert_with(|| types::responses::ResponseBuilder::new(response_id, String::new()));
			// Finished items, sorted into output order below: what `response.completed` carries.
			let mut output: Vec<(u32, responses::OutputItem)> = Vec::new();
			if let Some((item_id, output_index, text)) = self.reasoning.take() {
				let done_text = text.clone();
				let done_id = item_id.clone();
				self.push(out, |sequence_number| {
					E::ResponseReasoningTextDone(responses::ResponseReasoningTextDoneEvent {
						sequence_number,
						item_id: done_id,
						output_index,
						content_index: 0,
						text: done_text,
					})
				});
				let mut item = reasoning_item(text, responses::OutputStatus::Completed);
				if let responses::OutputItem::Reasoning(reasoning) = &mut item {
					reasoning.id = Some(item_id);
				}
				let done = item.clone();
				self.push(out, |sequence_number| {
					E::ResponseOutputItemDone(responses::ResponseOutputItemDoneEvent {
						sequence_number,
						output_index,
						item: done,
					})
				});
				output.push((output_index, item));
			}

			let message_output = self.open_message(out);
			let mut calls: Vec<_> = self.tool_calls.drain().map(|(_, call)| call).collect();
			calls.sort_by_key(|call| call.output_index);
			for call in calls {
				let ToolCall {
					id,
					name,
					arguments,
					output_index,
				} = call;
				let (done_id, done_name, done_arguments) = (id.clone(), name.clone(), arguments.clone());
				self.push(out, |sequence_number| {
					E::ResponseFunctionCallArgumentsDone(responses::ResponseFunctionCallArgumentsDoneEvent {
						sequence_number,
						output_index,
						name: Some(done_name),
						item_id: done_id,
						arguments: done_arguments,
					})
				});
				let item = function_call(id, name, arguments, responses::OutputStatus::Completed);
				let done = item.clone();
				self.push(out, |sequence_number| {
					E::ResponseOutputItemDone(responses::ResponseOutputItemDoneEvent {
						sequence_number,
						output_index,
						item: done,
					})
				});
				output.push((output_index, item));
			}

			let mut content = Vec::new();
			if self.sent_content_part {
				let text = self.text.clone();
				let item_id = self.message_id.clone();
				let (done_text, done_id) = (text.clone(), item_id.clone());
				self.push(out, |sequence_number| {
					E::ResponseOutputTextDone(responses::ResponseTextDoneEvent {
						sequence_number,
						item_id: done_id,
						output_index: message_output,
						content_index: 0,
						text: done_text,
						logprobs: None,
					})
				});
				let part_text = text.clone();
				self.push(out, |sequence_number| {
					E::ResponseContentPartDone(responses::ResponseContentPartDoneEvent {
						sequence_number,
						item_id,
						output_index: message_output,
						content_index: 0,
						part: responses::OutputContent::OutputText(responses::OutputTextContent {
							annotations: Vec::new(),
							logprobs: None,
							text: part_text,
						}),
					})
				});
				content.push(responses::OutputMessageContent::OutputText(
					responses::OutputTextContent {
						annotations: Vec::new(),
						logprobs: None,
						text,
					},
				));
			}
			let message = responses::OutputItem::Message(responses::OutputMessage {
				content,
				id: self.message_id.clone(),
				role: responses::AssistantRole::Assistant,
				phase: None,
				status: responses::OutputStatus::Completed,
			});
			let done = message.clone();
			self.push(out, |sequence_number| {
				E::ResponseOutputItemDone(responses::ResponseOutputItemDoneEvent {
					sequence_number,
					output_index: message_output,
					item: done,
				})
			});
			output.push((message_output, message));
			output.sort_by_key(|(output_index, _)| *output_index);
			let output: Vec<_> = output.into_iter().map(|(_, item)| item).collect();

			let builder = self.builder.as_ref().expect("set above");
			let usage = usage.map(response_usage);
			let sequence_number = self.sequence + 1;
			let mut done = match status(stop_reason) {
				(responses::Status::Incomplete, _, Some(details)) => {
					builder.incomplete_event(sequence_number, usage, details)
				},
				(responses::Status::Failed, Some(error), _) => {
					builder.failed_event(sequence_number, usage, error)
				},
				_ => builder.completed_event(sequence_number, usage),
			};
			match &mut done {
				E::ResponseCompleted(e) => e.response.output = output,
				E::ResponseIncomplete(e) => e.response.output = output,
				E::ResponseFailed(e) => e.response.output = output,
				_ => {},
			}
			self.push(out, |_| done);
		}
	}

	let mut items = Items {
		sequence: 0,
		response_id: random_id("resp"),
		message_id: random_id("msg"),
		builder: None,
		next_output: 0,
		message_output: None,
		reasoning: None,
		tool_calls: HashMap::new(),
		sent_content_part: false,
		text: String::new(),
		ended: false,
	};
	crate::parse::sse::json_transform_multi::<I, E, _>(body, buffer_limit, move |event| {
		let mut out = Vec::new();
		for event in parse(event) {
			items.write(event, &mut out);
		}
		if let Some(namespaces) = &namespaces {
			for (_, event) in &mut out {
				namespaces.restore_event(event);
			}
		}
		out
	})
}
