//! Local rendering and the vLLM v0.25.1 Rust gRPC worker protocol.
//!
//! The gateway renders and tokenizes chat requests once, routes using those
//! exact prompt token IDs, and sends the same IDs to the selected vLLM pod via
//! `GenerateStream`. The vLLM Rust frontend returns incremental text deltas,
//! which are exposed as an OpenAI-compatible SSE stream.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum_core::body::Body;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use hf_hub::api::tokio::{ApiBuilder, ApiRepo};
use hf_hub::{Repo, RepoType};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tonic::Status;
use tracing::warn;
use vllm_chat::{
	ChatContent, ChatContentPart, ChatMessage, ChatOptions, ChatRequest, ChatRole, DynChatRenderer,
	LoadModelBackendsOptions, SamplingParams, load_model_backends,
};
use vllm_text::Prompt;
use vllm_text::tokenizer::DynTokenizer;

use crate::types::completions::{Content, Request, RequestMessage};

/// The protobuf and generated gRPC client pinned to vLLM v0.25.1.
pub mod grpc {
	tonic::include_proto!("vllm");
}

/// Default gRPC port exposed by a managed-model pod.
pub const GRPC_PORT: u16 = 50051;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("failed to load or render the vLLM frontend: {0}")]
	Frontend(#[from] vllm_chat::Error),
	#[error("failed to tokenize the vLLM request: {0}")]
	Tokenizer(#[from] vllm_text::tokenizer::TokenizerError),
	#[error("unsupported text-only vLLM request: {0}")]
	Unsupported(String),
	#[error("invalid vLLM request: {0}")]
	Invalid(String),
	#[error("failed to resolve Hugging Face model frontend files: {0}")]
	HuggingFace(String),
	#[error("vLLM Rust gRPC request failed: {0}")]
	Rpc(#[from] tonic::Status),
}

/// Model-specific renderer and tokenizer cached by the gateway.
#[derive(Clone)]
pub struct Frontend {
	model_id: Arc<str>,
	renderer: DynChatRenderer,
	tokenizer: DynTokenizer,
}

impl Frontend {
	/// Pull the small model frontend files from Hugging Face (or load a local
	/// directory) and initialize the official vLLM Rust renderer/tokenizer.
	pub async fn load(model_id: impl Into<String>) -> Result<Self, Error> {
		let model_id = model_id.into();
		let backends = load_model_backends(
			&model_id,
			LoadModelBackendsOptions {
				language_model_only: true,
				..Default::default()
			},
		)
		.await?;

		Ok(Self {
			model_id: model_id.into(),
			renderer: backends.chat_backend.chat_renderer(),
			tokenizer: backends.text_backend.tokenizer(),
		})
	}

	/// Load an immutable Hugging Face model revision.
	pub async fn load_hugging_face(
		repository: impl Into<String>,
		revision: Option<&str>,
		token: Option<&str>,
	) -> Result<Self, Error> {
		let repository = repository.into();
		if Path::new(&repository).is_dir() {
			return Self::load(repository).await;
		}
		let revision = revision.unwrap_or("main").to_string();

		let api = ApiBuilder::from_env()
			.with_progress(false)
			// Always override the cache's implicit token, including with None.
			.with_token(token.map(str::to_string))
			.build()
			.map_err(|error| Error::HuggingFace(error.to_string()))?;
		let repo = api.repo(Repo::with_revision(
			repository.clone(),
			RepoType::Model,
			revision.clone(),
		));
		let info = repo.info().await.map_err(|error| {
			Error::HuggingFace(format!(
				"failed to inspect '{repository}@{revision}': {error}"
			))
		})?;
		let siblings = info
			.siblings
			.into_iter()
			.map(|sibling| sibling.rfilename)
			.collect::<BTreeSet<_>>();
		let filenames = frontend_filenames(&repository, &revision, &siblings)?;
		let mut root = None;
		for filename in filenames {
			let path = download_frontend_file(&repo, &repository, &revision, &filename).await?;
			if filename == "config.json" {
				root = path.parent().map(PathBuf::from);
			}
		}
		let root = root.ok_or_else(|| {
			Error::HuggingFace(format!(
				"downloaded '{repository}@{revision}' without a snapshot directory"
			))
		})?;
		let root = root
			.to_str()
			.ok_or_else(|| Error::HuggingFace("HF cache path is not valid UTF-8".to_string()))?;

		let mut frontend = Self::load(root).await?;
		frontend.model_id = repository.into();
		Ok(frontend)
	}

	pub fn from_parts(
		model_id: impl Into<Arc<str>>,
		renderer: DynChatRenderer,
		tokenizer: DynTokenizer,
	) -> Self {
		Self {
			model_id: model_id.into(),
			renderer,
			tokenizer,
		}
	}

	pub fn model_id(&self) -> &str {
		&self.model_id
	}

	/// Render and tokenize once. The returned token IDs are both the routing
	/// key and the prompt sent to the selected worker.
	pub fn prepare(
		&self,
		request_id: impl Into<String>,
		served_model: impl Into<String>,
		request: &Request,
	) -> Result<PreparedRequest, Error> {
		validate_text_request(request)?;

		let request_id = request_id.into();
		let served_model = served_model.into();
		let messages = request
			.messages
			.iter()
			.map(convert_message)
			.collect::<Result<Vec<_>, _>>()?;
		if messages.is_empty() {
			return Err(Error::Invalid("messages must not be empty".to_string()));
		}

		let mut template_kwargs = HashMap::new();
		if let Some(value) = request.rest.get("chat_template_kwargs") {
			template_kwargs = serde_json::from_value(value.clone()).map_err(|error| {
				Error::Invalid(format!("chat_template_kwargs must be an object: {error}"))
			})?;
		}
		let chat_template = request
			.rest
			.get("chat_template")
			.map(|value| {
				value
					.as_str()
					.map(str::to_string)
					.ok_or_else(|| Error::Invalid("chat_template must be a string".to_string()))
			})
			.transpose()?;

		let render_request = ChatRequest {
			request_id: request_id.clone(),
			messages,
			sampling_params: SamplingParams {
				temperature: request.temperature,
				top_p: request.top_p,
				seed: request.seed,
				max_tokens: request.max_completion_tokens.or(request.max_tokens),
				frequency_penalty: request.frequency_penalty,
				presence_penalty: request.presence_penalty,
				..Default::default()
			},
			chat_options: ChatOptions {
				chat_template,
				template_kwargs,
				..Default::default()
			},
			tools: Vec::new(),
			tool_choice: Default::default(),
			parallel_tool_calls: false,
			decode_options: Default::default(),
			intermediate: true,
			priority: 0,
			documents: None,
			cache_salt: None,
			add_special_tokens: false,
			data_parallel_rank: None,
			lora_request: None,
		};
		render_request.validate()?;

		let rendered = self.renderer.render(&render_request)?;
		let token_ids = match rendered.prompt {
			Prompt::Text(text) => self
				.tokenizer
				.encode(&text, render_request.add_special_tokens)?,
			Prompt::TokenIds(token_ids) => token_ids,
		};
		if token_ids.is_empty() {
			return Err(Error::Invalid(
				"rendered prompt produced no token IDs".to_string(),
			));
		}

		let parameters = worker_parameters(request)?;
		let response_id = if request_id.starts_with("chatcmpl-") {
			request_id.clone()
		} else {
			format!("chatcmpl-{request_id}")
		};
		let include_usage = request
			.stream_options
			.as_ref()
			.is_some_and(|options| options.include_usage);

		Ok(PreparedRequest {
			response_id,
			created: SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap_or_default()
				.as_secs(),
			include_usage,
			worker: grpc::GenerateRequest {
				request_id,
				model: served_model,
				prompt: Some(grpc::generate_request::Prompt::TokenIds(grpc::TokenIds {
					ids: token_ids,
				})),
				temperature: request.temperature,
				sampling: Some(parameters.sampling),
				decoding: Some(parameters.decoding),
				stopping: Some(parameters.stopping),
				response: Some(grpc::ResponseOptions {
					prompt_token_ids: false,
					prompt_logprobs: false,
					prompt_candidates: None,
					output_text: Some(true),
					output_token_ids: false,
					output_logprobs: false,
					output_candidates: None,
				}),
				kv: Some(parameters.kv),
				truncate_prompt_tokens: 0,
				priority: parameters.priority,
			},
		})
	}
}

/// Immutable result of local rendering.
#[derive(Debug, Clone)]
pub struct PreparedRequest {
	response_id: String,
	created: u64,
	include_usage: bool,
	worker: grpc::GenerateRequest,
}

impl PreparedRequest {
	pub fn token_ids(&self) -> &[u32] {
		match self.worker.prompt.as_ref() {
			Some(grpc::generate_request::Prompt::TokenIds(ids)) => &ids.ids,
			_ => &[],
		}
	}

	pub fn worker_request(&self) -> &grpc::GenerateRequest {
		&self.worker
	}

	/// Translate a native vLLM GenerateStream response into OpenAI SSE. The
	/// caller owns transport construction so agentgateway can use its existing
	/// policy-aware pooled gRPC channel.
	pub fn translate_stream<S>(&self, responses: S) -> Body
	where
		S: Stream<Item = Result<grpc::GenerateResponse, Status>> + Send + Unpin + 'static,
	{
		translate_responses(responses, self.clone())
	}
}

fn translate_responses<S>(mut responses: S, prepared: PreparedRequest) -> Body
where
	S: Stream<Item = Result<grpc::GenerateResponse, Status>> + Send + Unpin + 'static,
{
	let output = async_stream::stream! {
		let mut started = HashSet::new();
		let mut prompt_tokens = prepared.token_ids().len() as u32;
		let mut completion_tokens = 0;
		let mut failed = false;

		while let Some(response) = responses.next().await {
			let response = match response {
				Ok(response) => response,
				Err(error) => {
					warn!(%error, "vLLM Rust gRPC stream failed");
					yield Ok::<Bytes, Infallible>(sse_json(&StreamEvent::Error {
						error: StreamError {
							message: error.message().to_string(),
							r#type: "upstream_error",
						},
					}));
					failed = true;
					break;
				},
			};

			if let Some(prompt) = response.prompt_info {
				prompt_tokens = prompt.num_prompt_tokens;
			}
			let Some(output) = response.outputs else {
				continue;
			};

			let role = started.insert(output.index).then_some("assistant");
			let finish_reason = output
				.finish_info
				.as_ref()
				.and_then(|finish| finish_reason(finish.finish_reason));
			if let Some(finish) = &output.finish_info {
				completion_tokens = completion_tokens.max(finish.num_output_tokens);
			} else {
				completion_tokens = completion_tokens.saturating_add(output.num_tokens);
			}

			if role.is_none() && output.text.is_empty() && finish_reason.is_none() {
				continue;
			}
			yield Ok::<Bytes, Infallible>(sse_json(&StreamEvent::Chunk(OpenAiChunk {
				id: prepared.response_id.clone(),
				object: "chat.completion.chunk",
				created: prepared.created,
				model: prepared.worker.model.clone(),
				choices: vec![OpenAiChoice {
					index: output.index,
					delta: OpenAiDelta {
						role,
						content: (!output.text.is_empty()).then_some(output.text),
					},
					finish_reason,
				}],
				usage: None,
			})));
		}

		if prepared.include_usage && !failed {
			yield Ok::<Bytes, Infallible>(sse_json(&StreamEvent::Chunk(OpenAiChunk {
				id: prepared.response_id.clone(),
				object: "chat.completion.chunk",
				created: prepared.created,
				model: prepared.worker.model.clone(),
				choices: vec![],
				usage: Some(GenerateUsage {
					prompt_tokens,
					completion_tokens,
					total_tokens: prompt_tokens.saturating_add(completion_tokens),
				}),
			})));
		}
		yield Ok::<Bytes, Infallible>(crate::parse::encode_sse_event("", Bytes::from_static(b"[DONE]")));
	};
	Body::from_stream(output)
}

fn finish_reason(reason: i32) -> Option<&'static str> {
	match grpc::finish_info::FinishReason::try_from(reason).ok()? {
		grpc::finish_info::FinishReason::NotFinished => None,
		grpc::finish_info::FinishReason::Length => Some("length"),
		grpc::finish_info::FinishReason::Stop => Some("stop"),
		grpc::finish_info::FinishReason::Aborted => Some("error"),
	}
}

fn sse_json(value: &impl Serialize) -> Bytes {
	let json = serde_json::to_vec(value).expect("OpenAI stream event serialization");
	crate::parse::encode_sse_event("", Bytes::from(json))
}

fn validate_text_request(request: &Request) -> Result<(), Error> {
	if request.stream != Some(true) {
		return Err(Error::Unsupported(
			"vllm currently requires stream=true".to_string(),
		));
	}
	for unsupported in [
		"response_format",
		"reasoning_effort",
		"add_generation_prompt",
		"continue_final_message",
		"modalities",
		"audio",
		"logprobs",
		"top_logprobs",
		"best_of",
	] {
		if request
			.rest
			.get(unsupported)
			.is_some_and(|value| !value.is_null())
		{
			return Err(Error::Unsupported(format!(
				"request option '{unsupported}' is not implemented by the text-only MVP"
			)));
		}
	}
	let tool_choice_requested = request
		.tool_choice
		.as_ref()
		.is_some_and(|choice| choice.as_str() != Some("none") && !choice.is_null());
	if request
		.tools
		.as_ref()
		.is_some_and(|tools| !tools.is_empty())
		|| tool_choice_requested
	{
		return Err(Error::Unsupported(
			"tool calling is not implemented; this MVP streams assistant text only".to_string(),
		));
	}
	Ok(())
}

fn frontend_filenames(
	repository: &str,
	revision: &str,
	siblings: &BTreeSet<String>,
) -> Result<Vec<String>, Error> {
	if !siblings.contains("config.json") {
		return Err(Error::HuggingFace(format!(
			"model '{repository}@{revision}' has no config.json"
		)));
	}

	let tokenizer = ["tekken.json", "tokenizer.json", "tiktoken.model"]
		.into_iter()
		.find(|filename| siblings.contains(*filename))
		.map(str::to_string)
		.or_else(|| {
			siblings
				.iter()
				.find(|filename| filename.ends_with(".tiktoken"))
				.cloned()
		})
		.ok_or_else(|| {
			Error::HuggingFace(format!(
				"model '{repository}@{revision}' has no supported tokenizer file"
			))
		})?;

	let mut filenames = vec!["config.json".to_string(), tokenizer];
	for optional in [
		"tokenizer_config.json",
		"generation_config.json",
		"chat_template.json",
		"chat_template.jinja",
	] {
		if siblings.contains(optional) {
			filenames.push(optional.to_string());
		}
	}
	Ok(filenames)
}

async fn download_frontend_file(
	repo: &ApiRepo,
	repository: &str,
	revision: &str,
	filename: &str,
) -> Result<PathBuf, Error> {
	repo.get(filename).await.map_err(|error| {
		Error::HuggingFace(format!(
			"failed to download '{filename}' for '{repository}@{revision}': {error}"
		))
	})
}

fn convert_message(message: &RequestMessage) -> Result<ChatMessage, Error> {
	if message.name.is_some() || message.tool_call_id.is_some() || message.tool_calls.is_some() {
		return Err(Error::Unsupported(
			"message names, tool calls, and tool responses are not implemented".to_string(),
		));
	}

	let content = convert_content(message.content.as_ref())?;
	match message.role.as_str() {
		"system" => Ok(ChatMessage::System { content }),
		"developer" => Ok(ChatMessage::Developer {
			content,
			tools: None,
		}),
		"user" => Ok(ChatMessage::User { content }),
		"assistant" => Ok(ChatMessage::text(
			ChatRole::Assistant,
			content.try_flatten_to_text()?,
		)),
		role => Err(Error::Unsupported(format!(
			"message role '{role}' is not supported by the text-only MVP"
		))),
	}
}

fn convert_content(content: Option<&Content>) -> Result<ChatContent, Error> {
	match content {
		None => Ok(ChatContent::Text(String::new())),
		Some(Content::Text(text)) => Ok(ChatContent::Text(text.clone())),
		Some(Content::Array(parts)) => parts
			.iter()
			.map(|part| {
				if part.r#type != "text" {
					return Err(Error::Unsupported(format!(
						"content part '{}' is not implemented; only text is supported",
						part.r#type
					)));
				}
				part
					.text
					.clone()
					.map(ChatContentPart::text)
					.ok_or_else(|| Error::Invalid("text content part is missing text".to_string()))
			})
			.collect::<Result<Vec<_>, _>>()
			.map(ChatContent::Parts),
	}
}

struct WorkerParameters {
	sampling: grpc::RandomSampling,
	decoding: grpc::DecodingParameters,
	stopping: grpc::StoppingCriteria,
	kv: grpc::KvCacheParameters,
	priority: i32,
}

fn worker_parameters(request: &Request) -> Result<WorkerParameters, Error> {
	let mut rest = match &request.rest {
		Value::Null => Map::new(),
		Value::Object(map) => map.clone(),
		_ => {
			return Err(Error::Invalid(
				"flattened request options must be an object".to_string(),
			));
		},
	};

	for frontend_only in [
		"chat_template",
		"chat_template_kwargs",
		"parallel_tool_calls",
		"metadata",
		"service_tier",
		"response_format",
		"reasoning_effort",
		"add_generation_prompt",
		"continue_final_message",
		"modalities",
		"audio",
		"logprobs",
		"top_logprobs",
		"best_of",
	] {
		rest.remove(frontend_only);
	}

	let top_k = take::<u32>(&mut rest, "top_k")?.unwrap_or_default();
	let min_p = take::<f32>(&mut rest, "min_p")?.unwrap_or_default();
	let n = take::<u32>(&mut rest, "n")?.unwrap_or(1);
	if n > 1 {
		return Err(Error::Unsupported(
			"n > 1 is not supported by vLLM v0.25.1 GenerateStream".to_string(),
		));
	}
	let repetition_penalty = take::<f32>(&mut rest, "repetition_penalty")?.unwrap_or_default();
	let min_new_tokens = take::<u32>(&mut rest, "min_tokens")?.unwrap_or_default();
	let stop_token_ids = take::<Vec<u32>>(&mut rest, "stop_token_ids")?.unwrap_or_default();
	let ignore_eos = take::<bool>(&mut rest, "ignore_eos")?.unwrap_or_default();
	let include_stop_strings =
		take::<bool>(&mut rest, "include_stop_str_in_output")?.unwrap_or_default();
	let allowed_token_ids = take::<Vec<u32>>(&mut rest, "allowed_token_ids")?.unwrap_or_default();
	let logit_bias = take_logit_bias(&mut rest)?;
	let cache_salt = take::<String>(&mut rest, "cache_salt")?.unwrap_or_default();
	let bypass_prefix_cache =
		take::<bool>(&mut rest, "skip_reading_prefix_cache")?.unwrap_or_default();
	let priority = take::<i64>(&mut rest, "priority")?.unwrap_or_default();
	let priority = i32::try_from(priority)
		.map_err(|_| Error::Invalid("priority must fit in a signed 32-bit integer".to_string()))?;

	if let Some(skip_special_tokens) = take::<bool>(&mut rest, "skip_special_tokens")?
		&& !skip_special_tokens
	{
		return Err(Error::Unsupported(
			"skip_special_tokens=false is not supported by the vLLM Rust gRPC API".to_string(),
		));
	}
	rest.remove("spaces_between_special_tokens");
	if let Some((name, _)) = rest.iter().next() {
		return Err(Error::Unsupported(format!(
			"request option '{name}' is not supported by the vLLM Rust gRPC API"
		)));
	}

	Ok(WorkerParameters {
		sampling: grpc::RandomSampling {
			num_sequences: n,
			top_k,
			top_p: request.top_p.unwrap_or_default(),
			min_p,
			seed: request.seed,
		},
		decoding: grpc::DecodingParameters {
			presence_penalty: request.presence_penalty.unwrap_or_default(),
			frequency_penalty: request.frequency_penalty.unwrap_or_default(),
			repetition_penalty,
			logit_bias,
			allowed_token_ids,
			structured_output: None,
		},
		stopping: grpc::StoppingCriteria {
			max_new_tokens: request
				.max_completion_tokens
				.or(request.max_tokens)
				.unwrap_or_default(),
			min_new_tokens,
			stop_token_ids,
			stop_strings: stop_strings(request.stop.as_ref())?,
			include_stop_strings,
			ignore_eos,
		},
		kv: grpc::KvCacheParameters {
			bypass_prefix_cache,
			cache_salt,
			kv_transfer_params: None,
		},
		priority,
	})
}

fn take<T: DeserializeOwned>(map: &mut Map<String, Value>, key: &str) -> Result<Option<T>, Error> {
	map
		.remove(key)
		.map(|value| {
			serde_json::from_value(value)
				.map_err(|error| Error::Invalid(format!("invalid '{key}': {error}")))
		})
		.transpose()
}

fn take_logit_bias(map: &mut Map<String, Value>) -> Result<HashMap<u32, f32>, Error> {
	let Some(value) = map.remove("logit_bias") else {
		return Ok(HashMap::new());
	};
	let values: HashMap<String, f32> = serde_json::from_value(value)
		.map_err(|error| Error::Invalid(format!("invalid 'logit_bias': {error}")))?;
	values
		.into_iter()
		.map(|(token, bias)| {
			let token = token.parse::<u32>().map_err(|error| {
				Error::Invalid(format!("logit_bias token '{token}' is not a u32: {error}"))
			})?;
			Ok((token, bias))
		})
		.collect()
}

fn stop_strings(stop: Option<&Value>) -> Result<Vec<String>, Error> {
	match stop {
		None | Some(Value::Null) => Ok(Vec::new()),
		Some(Value::String(stop)) => Ok(vec![stop.clone()]),
		Some(value @ Value::Array(_)) => serde_json::from_value(value.clone())
			.map_err(|error| Error::Invalid(format!("stop must contain only strings: {error}"))),
		Some(_) => Err(Error::Invalid(
			"stop must be a string or an array of strings".to_string(),
		)),
	}
}

#[derive(Serialize)]
#[serde(untagged)]
enum StreamEvent {
	Chunk(OpenAiChunk),
	Error { error: StreamError },
}

#[derive(Serialize)]
struct StreamError {
	message: String,
	r#type: &'static str,
}

#[derive(Serialize)]
struct OpenAiChunk {
	id: String,
	object: &'static str,
	created: u64,
	model: String,
	choices: Vec<OpenAiChoice>,
	#[serde(skip_serializing_if = "Option::is_none")]
	usage: Option<GenerateUsage>,
}

#[derive(Serialize)]
struct OpenAiChoice {
	index: u32,
	delta: OpenAiDelta,
	finish_reason: Option<&'static str>,
}

#[derive(Serialize)]
struct OpenAiDelta {
	#[serde(skip_serializing_if = "Option::is_none")]
	role: Option<&'static str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	content: Option<String>,
}

#[derive(Serialize)]
struct GenerateUsage {
	prompt_tokens: u32,
	completion_tokens: u32,
	total_tokens: u32,
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use futures_util::stream;
	use http_body_util::BodyExt;
	use serde_json::json;
	use vllm_chat::{ChatRenderer, RenderedPrompt};
	use vllm_text::tokenizer::{Result as TokenizerResult, Tokenizer};

	use super::*;
	use crate::types::completions::StreamOptions;

	#[derive(Debug)]
	struct ByteTokenizer;

	impl Tokenizer for ByteTokenizer {
		fn encode(&self, text: &str, _add_special_tokens: bool) -> TokenizerResult<Vec<u32>> {
			Ok(text.bytes().map(u32::from).collect())
		}

		fn decode(&self, token_ids: &[u32], _skip_special_tokens: bool) -> TokenizerResult<String> {
			let bytes = token_ids.iter().map(|id| *id as u8).collect::<Vec<_>>();
			Ok(String::from_utf8_lossy(&bytes).into_owned())
		}

		fn token_to_id(&self, token: &str) -> Option<u32> {
			(token.len() == 1).then(|| token.as_bytes()[0] as u32)
		}

		fn id_to_token(&self, id: u32) -> Option<String> {
			Some((id as u8 as char).to_string())
		}
	}

	struct TestRenderer;

	impl ChatRenderer for TestRenderer {
		fn render(&self, request: &ChatRequest) -> vllm_chat::Result<RenderedPrompt> {
			let text = request
				.messages
				.iter()
				.map(ChatMessage::text_content)
				.collect::<vllm_chat::Result<Vec<_>>>()?
				.join("|");
			Ok(RenderedPrompt {
				prompt: Prompt::Text(format!("prompt:{text}:assistant:")),
				effective_template_kwargs: HashMap::new(),
			})
		}
	}

	fn frontend() -> Frontend {
		Frontend::from_parts("org/model", Arc::new(TestRenderer), Arc::new(ByteTokenizer))
	}

	fn request() -> Request {
		Request {
			messages: vec![RequestMessage {
				role: "user".to_string(),
				name: None,
				content: Some(Content::Text("hello".to_string())),
				tool_call_id: None,
				tool_calls: None,
				rest: json!({}),
			}],
			model: Some("alias".to_string()),
			top_p: Some(0.9),
			temperature: Some(0.2),
			stream: Some(true),
			frequency_penalty: None,
			presence_penalty: None,
			seed: Some(7),
			stream_options: Some(StreamOptions {
				include_usage: true,
				rest: json!({}),
			}),
			max_tokens: Some(32),
			max_completion_tokens: None,
			stop: None,
			tools: None,
			tool_choice: None,
			user: None,
			rest: json!({"top_k": 20}),
		}
	}

	#[test]
	fn prepare_renders_once_and_builds_grpc_request() {
		let prepared = frontend()
			.prepare("req-1", "served-model", &request())
			.unwrap();

		assert_eq!(
			prepared.token_ids(),
			b"prompt:hello:assistant:"
				.iter()
				.map(|byte| *byte as u32)
				.collect::<Vec<_>>()
		);
		let worker = prepared.worker_request();
		assert_eq!(worker.request_id, "req-1");
		assert_eq!(worker.model, "served-model");
		assert_eq!(worker.temperature, Some(0.2));
		assert_eq!(worker.sampling.as_ref().unwrap().top_k, 20);
		assert_eq!(worker.sampling.as_ref().unwrap().top_p, 0.9);
		assert_eq!(worker.sampling.as_ref().unwrap().seed, Some(7));
		assert_eq!(worker.stopping.as_ref().unwrap().max_new_tokens, 32);
		assert_eq!(worker.response.as_ref().unwrap().output_text, Some(true));
	}

	#[tokio::test]
	async fn translates_grpc_deltas_to_openai_text_sse() {
		let prepared = frontend()
			.prepare("req-1", "served-model", &request())
			.unwrap();
		let responses = stream::iter([
			Ok(grpc::GenerateResponse {
				prompt_info: Some(grpc::PromptInfo {
					num_prompt_tokens: 23,
					..Default::default()
				}),
				outputs: None,
			}),
			Ok(grpc::GenerateResponse {
				prompt_info: None,
				outputs: Some(grpc::SequenceOutput {
					index: 0,
					text: "Hi".to_string(),
					num_tokens: 2,
					finish_info: None,
					..Default::default()
				}),
			}),
			Ok(grpc::GenerateResponse {
				prompt_info: None,
				outputs: Some(grpc::SequenceOutput {
					index: 0,
					finish_info: Some(grpc::FinishInfo {
						num_output_tokens: 2,
						finish_reason: grpc::finish_info::FinishReason::Stop as i32,
						..Default::default()
					}),
					..Default::default()
				}),
			}),
		]);

		let body = translate_responses(responses, prepared)
			.collect()
			.await
			.unwrap()
			.to_bytes();
		let body = String::from_utf8(body.to_vec()).unwrap();
		assert!(
			body.contains("\"role\":\"assistant\",\"content\":\"Hi\""),
			"{body}"
		);
		assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
		assert!(body.contains("\"prompt_tokens\":23"), "{body}");
		assert!(body.contains("\"completion_tokens\":2"), "{body}");
		assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
	}

	#[test]
	fn revision_loader_selects_only_frontend_files() {
		let siblings = [
			"config.json",
			"tokenizer.json",
			"tokenizer_config.json",
			"generation_config.json",
			"model.safetensors",
		]
		.into_iter()
		.map(str::to_string)
		.collect();

		assert_eq!(
			frontend_filenames("org/model", "deadbeef", &siblings).unwrap(),
			vec![
				"config.json",
				"tokenizer.json",
				"tokenizer_config.json",
				"generation_config.json",
			]
		);
	}

	#[test]
	fn rejects_multimodal_content_for_text_mvp() {
		let mut request = request();
		request.messages[0].content = Some(Content::Array(vec![
			crate::types::completions::ContentPart {
				r#type: "image_url".to_string(),
				text: None,
				rest: json!({"image_url": {"url": "https://example.com/a.png"}}),
			},
		]));

		let error = frontend()
			.prepare("req-1", "served-model", &request)
			.unwrap_err();
		assert!(error.to_string().contains("only text is supported"));
	}
}
