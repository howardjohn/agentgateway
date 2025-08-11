use agent_core::strng;
use agent_core::strng::Strng;
use anyhow::anyhow;
use bytes::Bytes;
use serde_json::Value;
use tiktoken_rs::CoreBPE;
use tiktoken_rs::tokenizer::{Tokenizer, get_tokenizer};

use super::{LLMResponse, Provider as LLMProvider, universal};
use crate::http::{Body, Request, Response};
use crate::llm::{AIError, AIProvider, LLMRequest};
use crate::proxy::ProxyError;
use crate::*;

#[apply(schema!)]
pub struct Provider {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub model: Option<Strng>,
}

impl super::Provider for Provider {
	const NAME: Strng = strng::literal!("openai");
}
pub const DEFAULT_HOST_STR: &str = "api.openai.com";
pub const DEFAULT_HOST: Strng = strng::literal!(DEFAULT_HOST_STR);
pub const DEFAULT_PATH: &str = "/v1/chat/completions";

impl Provider {
	pub async fn process_request(
		&self,
		mut req: universal::Request,
	) -> Result<universal::Request, AIError> {
		if let Some(model) = &self.model {
			req.model = model.to_string();
		}
		// This is openai already...
		Ok(req)
	}
	pub async fn process_response(&self, bytes: &Bytes) -> Result<universal::Response, AIError> {
		let resp =
			serde_json::from_slice::<universal::Response>(bytes).map_err(AIError::ResponseParsing)?;
		Ok(resp)
	}
	pub async fn process_error(
		&self,
		bytes: &Bytes,
	) -> Result<universal::ChatCompletionErrorResponse, AIError> {
		let resp = serde_json::from_slice::<universal::ChatCompletionErrorResponse>(bytes)
			.map_err(AIError::ResponseParsing)?;
		Ok(resp)
	}
}
