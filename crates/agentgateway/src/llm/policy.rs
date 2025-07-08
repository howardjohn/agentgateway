use std::num::NonZeroU8;
use std::ops::Deref;

use ::http::HeaderMap;
use bytes::Bytes;

use crate::http::{PolicyResponse, Response, StatusCode};
use crate::llm::policy::webhook::{MaskActionBody, RequestAction};
use crate::llm::universal::{ChatCompletionMessage, ChatCompletionRequest, Content, MessageRole};
use crate::llm::{anthropic, bedrock, gemini, openai, pii, vertex};
use crate::proxy::ProxyError;
use crate::types::agent::Target;
use crate::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
	prompt_guard: Option<PromptGuard>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptGuard {
	request: Option<PromptGuardRequest>,
}
impl Policy {
	pub async fn apply(
		&self,
		client: client::Client,
		req: &mut ChatCompletionRequest,
		http_headers: &HeaderMap,
	) -> anyhow::Result<Option<Response>> {
		let Some(g) = self.prompt_guard.as_ref().and_then(|g| g.request.as_ref()) else {
			return Ok(None);
		};
		if let Some(webhook) = &g.webhook {
			let whr = webhook::send_request(client.clone(), &webhook.target, http_headers, req).await?;
			match whr.action {
				RequestAction::Mask(mask) => {
					debug!(
						"webhook masked: {}",
						mask
							.reason
							.unwrap_or_else(|| "no reason specified".to_string())
					);
					let MaskActionBody::PromptMessages(body) = mask.body else {
						anyhow::bail!("invalid webhook response");
					};
					let msgs = body.messages;
					req.messages = msgs
						.into_iter()
						.map(|r| ChatCompletionMessage {
							role: match r.role.as_str() {
								"user" => MessageRole::user,
								"system" => MessageRole::system,
								"assistant" => MessageRole::assistant,
								"function" => MessageRole::function,
								"tool" => MessageRole::tool,
								_ => MessageRole::user,
							},
							content: Content::Text(r.content),
							name: None,
							tool_calls: None,
							tool_call_id: None,
						})
						.collect();
				},
				RequestAction::Reject(rej) => {
					debug!(
						"webhook rejected: {}",
						rej
							.reason
							.unwrap_or_else(|| "no reason specified".to_string())
					);
					return Ok(Some(
						::http::response::Builder::new()
							.status(rej.status_code)
							.body(http::Body::from(rej.body))?,
					));
				},
				RequestAction::Pass(pass) => {
					debug!(
						"webhook passed: {}",
						pass
							.reason
							.unwrap_or_else(|| "no reason specified".to_string())
					);
					// No action needed
				},
			}
		}
		for msg in &mut req.messages {
			let mut content = msg.content.must_as_text().to_string();
			if let Some(rgx) = &g.regex {
				for r in &rgx.rules {
					match r {
						RegexRule::Builtin { builtin } => match builtin {
							Builtin::Ssn => pii::recognizer(pii::SSN.deref(), &content),
							Builtin::CreditCard => pii::recognizer(pii::CC.deref(), &content),
							Builtin::PhoneNumber => pii::recognizer(pii::PHONE.deref(), &content),
							Builtin::Email => pii::recognizer(pii::EMAIL.deref(), &content),
						},
						RegexRule::Regex { pattern, name } => {
							if let Some(m) = pattern.find(&content) {
								if let Action::Reject { response } = &rgx.action {
									return Ok(Some(response.as_response()));
								}
								let mut new_content = content.clone();
								new_content.replace_range(m.range(), &format!("<{name}>"));
								msg.content = Content::Text(new_content);
							}
						},
					}
				}
			}
		}
		Ok(None)
	}
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptGuardRequest {
	#[serde(default)]
	response: PromptGuardResponse,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	regex: Option<RegexRules>,
	webhook: Option<Webhook>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegexRules {
	#[serde(default)]
	response: PromptGuardResponse,
	#[serde(default, flatten)]
	action: Action,
	rules: Vec<RegexRule>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged, rename_all = "camelCase")]
pub enum RegexRule {
	Builtin {
		builtin: Builtin,
	},
	Regex {
		#[serde(with = "serde_regex")]
		pattern: regex::Regex,
		name: String,
	},
}

impl PromptGuardResponse {
	pub fn as_response(&self) -> Response {
		::http::response::Builder::new()
			.status(self.status)
			.body(http::Body::from(self.body.clone()))
			.expect("static request should succeed")
	}
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Builtin {
	#[serde(rename = "ssn")]
	Ssn,
	CreditCard,
	PhoneNumber,
	Email,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule<T> {
	action: Action,
	rule: T,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedRegex {
	#[serde(with = "serde_regex")]
	pattern: regex::Regex,
	name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Webhook {
	target: Target,
	// TODO: headers
}
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
	#[default]
	Mask,
	Reject {
		#[serde(default)]
		response: PromptGuardResponse,
	},
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptGuardResponse {
	#[serde(default = "default_body", serialize_with = "ser_string_or_bytes")]
	body: Bytes,
	#[serde(
		default = "default_code",
		serialize_with = "ser_display",
		deserialize_with = "de_parse"
	)]
	status: StatusCode,
}

impl Default for PromptGuardResponse {
	fn default() -> Self {
		Self {
			body: default_body(),
			status: default_code(),
		}
	}
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptGuardRegex {}
fn default_code() -> StatusCode {
	StatusCode::FORBIDDEN
}

fn default_body() -> Bytes {
	Bytes::from_static(b"The request was rejected due to inappropriate content")
}

mod webhook {
	use ::http::header::CONTENT_TYPE;
	use ::http::{HeaderMap, HeaderValue, header};
	use serde::{Deserialize, Serialize};

	use crate::client::Client;
	use crate::llm::universal::ChatCompletionRequest;
	use crate::types::agent::Target;
	use crate::*;

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct GuardrailsPromptRequest {
		/// body contains the object which is a list of the Message JSON objects from the prompts in the request
		pub body: PromptMessages,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct GuardrailsPromptResponse {
		/// action is the action to be taken based on the request.
		/// The following actions are available on the response:
		/// - PassAction: No action is required.
		/// - MaskAction: Mask the response body.
		/// - RejectAction: Reject the request.
		pub action: RequestAction,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct GuardrailsResponseRequest {
		/// body contains the object with a list of Choice that contains the response content from the LLM.
		pub body: ResponseChoices,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct GuardrailsResponseResponse {
		/// action is the action to be taken based on the request.
		/// The following actions are available on the response:
		/// - PassAction: No action is required.
		/// - MaskAction: Mask the response body.
		pub action: ResponseAction,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct Message {
		/// The role associated to the content in this message.
		pub role: String,
		/// The content text for this message.
		pub content: String,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct PromptMessages {
		/// List of prompt messages including role and content.
		pub messages: Vec<Message>,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct ResponseChoice {
		/// message contains the role and text content of the response from the LLM model.
		pub message: Message,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct ResponseChoices {
		/// list of possible independent responses from the LLM
		pub choices: Vec<ResponseChoice>,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct PassAction {
		/// reason is a human readable string that explains the reason for the action.
		#[serde(skip_serializing_if = "Option::is_none")]
		pub reason: Option<String>,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct MaskAction {
		/// body contains the modified messages that masked out some of the original contents.
		/// When used in a GuardrailPromptResponse, this should be PromptMessages.
		/// When used in GuardrailResponseResponse, this should be ResponseChoices
		pub body: MaskActionBody,
		/// reason is a human readable string that explains the reason for the action.
		#[serde(skip_serializing_if = "Option::is_none")]
		pub reason: Option<String>,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct RejectAction {
		/// body is the rejection message that will be used for HTTP error response body.
		pub body: String,
		/// status_code is the HTTP status code to be returned in the HTTP error response.
		pub status_code: u16,
		/// reason is a human readable string that explains the reason for the action.
		#[serde(skip_serializing_if = "Option::is_none")]
		pub reason: Option<String>,
	}

	/// Enum for actions available in prompt responses
	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(untagged, rename_all = "snake_case")]
	pub enum RequestAction {
		Mask(MaskAction),
		Reject(RejectAction),
		Pass(PassAction),
	}

	/// Enum for actions available in response responses
	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(untagged, rename_all = "snake_case")]
	pub enum ResponseAction {
		Mask(MaskAction),
		Pass(PassAction),
	}

	/// Enum for MaskAction body that can be either PromptMessages or ResponseChoices
	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(untagged)]
	pub enum MaskActionBody {
		PromptMessages(PromptMessages),
		ResponseChoices(ResponseChoices),
	}

	fn build_request_for_request(
		target: &Target,
		http_headers: &HeaderMap,
		i: &ChatCompletionRequest,
	) -> anyhow::Result<crate::http::Request> {
		let body = GuardrailsPromptRequest {
			body: PromptMessages {
				messages: i
					.messages
					.iter()
					.map(|m| Message {
						role: format!("{:?}", m.role),
						content: m.content.must_as_text().to_string(),
					})
					.collect(),
			},
		};
		let body_bytes = serde_json::to_vec(&body)?;
		let mut rb = ::http::Request::builder()
			.uri(format!("http://{target}/request"))
			.method(http::Method::POST);
		for (k, v) in http_headers {
			// TODO: this is configurable by users
			if k == header::CONTENT_LENGTH {
				// TODO: probably others
				continue;
			}
			rb = rb.header(k.clone(), v.clone());
		}
		let mut req = rb
			.header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
			.body(crate::http::Body::from(body_bytes))?;
		Ok(req)
	}

	pub async fn send_request(
		client: Client,
		target: &Target,
		http_headers: &HeaderMap,
		req: &ChatCompletionRequest,
	) -> anyhow::Result<GuardrailsPromptResponse> {
		let whr = build_request_for_request(target, http_headers, req)?;
		let res = client
			.call(client::Call {
				req: whr,
				target: target.clone(),
				transport: Default::default(), // TODO: use policies
			})
			.await?;
		let bb = axum::body::to_bytes(res.into_body(), 2_097_152).await?;
		let parsed = serde_json::from_slice::<GuardrailsPromptResponse>(&bb)?;
		Ok(parsed)
	}
}
