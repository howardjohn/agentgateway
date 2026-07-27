use std::sync::Arc;

use agent_core::prelude::Strng;
use agent_core::strng;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::OnceCell;
use tonic::Request as TonicRequest;

use crate::http::Body;
use crate::http::ext_proc::GrpcReferenceChannel;
use crate::proxy::httpproxy::PolicyClient;
use crate::types::agent::{SimpleBackendReference, Target};

/// A managed inference provider. Model-specific rendering and tokenization
/// metadata is initialized once, lazily, and shared by every request.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Provider {
	/// Model name sent to the vLLM worker. This is also the default source for
	/// the local renderer and tokenizer.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub model: Option<Strng>,
	/// Hugging Face repository or local model directory used for local request
	/// rendering when it differs from the served model name.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub frontend_model: Option<Strng>,
	/// Optional immutable Hugging Face revision for the frontend files.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub revision: Option<Strng>,
	/// Token used to download frontend metadata for private or gated Hugging
	/// Face repositories.
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		serialize_with = "crate::serdes::ser_redact",
		deserialize_with = "crate::serdes::deser_key_from_file_option"
	)]
	#[cfg_attr(
		feature = "schema",
		schemars(with = "Option<crate::serdes::FileOrInline>")
	)]
	pub hf_token: Option<SecretString>,
	#[serde(skip, default = "runtime_cell")]
	#[cfg_attr(feature = "schema", schemars(skip))]
	runtime: Arc<OnceCell<Runtime>>,
}

impl std::fmt::Debug for Provider {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Provider")
			.field("model", &self.model)
			.field("frontend_model", &self.frontend_model)
			.field("revision", &self.revision)
			.field("hf_token", &self.hf_token.as_ref().map(|_| "<redacted>"))
			.finish()
	}
}

impl Provider {
	pub fn uninitialized(model: Option<Strng>, hf_token: Option<SecretString>) -> Self {
		Self {
			model,
			frontend_model: None,
			revision: None,
			hf_token,
			runtime: runtime_cell(),
		}
	}

	pub fn from_config(
		model: Option<Strng>,
		frontend_model: Strng,
		revision: Option<Strng>,
		hf_token: Option<SecretString>,
	) -> Self {
		Self {
			model,
			frontend_model: Some(frontend_model),
			revision,
			hf_token,
			runtime: runtime_cell(),
		}
	}

	pub fn new(model: Option<Strng>, frontend: agent_llm::vllm::Frontend) -> Self {
		Self {
			model,
			frontend_model: None,
			revision: None,
			hf_token: None,
			runtime: Arc::new(OnceCell::new_with(Some(Runtime { frontend }))),
		}
	}

	async fn runtime(&self) -> Result<&Runtime, agent_llm::vllm::Error> {
		self
			.runtime
			.get_or_try_init(|| async {
				let frontend_model = self
					.frontend_model
					.as_ref()
					.or(self.model.as_ref())
					.ok_or_else(|| {
						agent_llm::vllm::Error::Invalid(
							"managed provider requires model or frontendModel".to_string(),
						)
					})?;
				let frontend = agent_llm::vllm::Frontend::load_hugging_face(
					frontend_model.to_string(),
					self.revision.as_deref(),
					self.hf_token.as_ref().map(ExposeSecret::expose_secret),
				)
				.await?;
				Ok(Runtime { frontend })
			})
			.await
	}

	pub async fn prepare(
		&self,
		request_id: impl Into<String>,
		served_model: impl Into<String>,
		request: &agent_llm::types::completions::Request,
	) -> Result<agent_llm::vllm::PreparedRequest, agent_llm::vllm::Error> {
		self
			.runtime()
			.await?
			.frontend
			.prepare(request_id, served_model, request)
	}

	pub async fn generate_stream(
		&self,
		client: PolicyClient,
		target: &Target,
		request: &agent_llm::vllm::PreparedRequest,
	) -> Result<Body, agent_llm::vllm::Error> {
		let channel = GrpcReferenceChannel {
			target: Arc::new(SimpleBackendReference::InlineBackend(target.clone())),
			client,
			policies: Arc::new(Vec::new()),
		};
		let mut client = agent_llm::vllm::grpc::generate_client::GenerateClient::new(channel);
		let response = client
			.generate_stream(TonicRequest::new(request.worker_request().clone()))
			.await?;
		Ok(request.translate_stream(response.into_inner()))
	}
}

impl agent_llm::Provider for Provider {
	const NAME: Strng = strng::literal!("managed");
}

struct Runtime {
	frontend: agent_llm::vllm::Frontend,
}

fn runtime_cell() -> Arc<OnceCell<Runtime>> {
	Arc::new(OnceCell::new())
}
