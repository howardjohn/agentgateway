use agent_core::strng;
use agent_core::strng::Strng;

use crate::llm::RouteType;
use crate::*;

#[apply(schema!)]
pub struct Provider {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub model: Option<Strng>,
}

impl super::Provider for Provider {
	const NAME: Strng = strng::literal!("copilot");
}

impl Provider {
	fn configured_model<'a>(&'a self, request_model: Option<&'a str>) -> Option<&'a str> {
		self.model.as_deref().or(request_model)
	}

	pub fn is_anthropic_model(&self, request_model: Option<&str>) -> bool {
		let Some(model) = self.configured_model(request_model) else {
			return false;
		};
		model.starts_with("claude-") || model.starts_with("anthropic/")
	}

	pub fn path_suffix(&self, route: RouteType, request_model: Option<&str>) -> &'static str {
		match route {
			RouteType::Completions if self.is_anthropic_model(request_model) => "/v1/messages",
			RouteType::AnthropicTokenCount if self.is_anthropic_model(request_model) => {
				"/v1/messages/count_tokens"
			},
			_ => path_suffix(route),
		}
	}
}

pub const DEFAULT_HOST_STR: &str = "api.githubcopilot.com";
pub const DEFAULT_HOST: Strng = strng::literal!(DEFAULT_HOST_STR);

pub fn path_suffix(route: RouteType) -> &'static str {
	match route {
		RouteType::Responses => "/responses",
		RouteType::Embeddings => "/embeddings",
		RouteType::Models => "/models",
		// note: the /v1 here and not on the others is how the API works...
		RouteType::Messages => "/v1/messages",
		RouteType::AnthropicTokenCount => "/v1/messages/count_tokens",
		_ => "/chat/completions",
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[rstest::rstest]
	#[case::raw_claude(None, Some("claude-sonnet-4-5"), true)]
	#[case::anthropic_prefix(None, Some("anthropic/claude-sonnet-4-5"), true)]
	#[case::provider_model(Some("claude-sonnet-4-5"), Some("gpt-4.1"), true)]
	#[case::openai_model(None, Some("gpt-4.1"), false)]
	fn test_is_anthropic_model(
		#[case] provider_model: Option<&str>,
		#[case] request_model: Option<&str>,
		#[case] expected: bool,
	) {
		let provider = Provider {
			model: provider_model.map(strng::new),
		};

		assert_eq!(provider.is_anthropic_model(request_model), expected);
	}

	#[test]
	fn test_anthropic_models_use_messages_paths() {
		let provider = Provider {
			model: Some(strng::new("claude-sonnet-4-5")),
		};

		assert_eq!(
			provider.path_suffix(RouteType::Completions, Some("gpt-4.1")),
			"/v1/messages"
		);
		assert_eq!(
			provider.path_suffix(RouteType::AnthropicTokenCount, Some("gpt-4.1")),
			"/v1/messages/count_tokens"
		);
	}
}
