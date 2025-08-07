use std::fmt::Debug;

use agent_core::metrics::{DefaultedUnknown, EncodeDisplay};
use agent_core::strng::RichStrng;
use agent_core::version;
use prometheus_client::encoding::{EncodeLabelSet, EncodeMetric};
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry;
use prometheus_client::registry::Registry;

use crate::types::agent::BindProtocol;

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct HTTPLabels {
	pub bind: DefaultedUnknown<RichStrng>,
	pub gateway: DefaultedUnknown<RichStrng>,
	pub listener: DefaultedUnknown<RichStrng>,
	pub route: DefaultedUnknown<RichStrng>,
	pub route_rule: DefaultedUnknown<RichStrng>,
	pub backend: DefaultedUnknown<RichStrng>,

	pub method: DefaultedUnknown<EncodeDisplay<http::Method>>,
	pub status: DefaultedUnknown<EncodeDisplay<u16>>,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct GenAILabels {
	gen_ai_operation_name: DefaultedUnknown<RichStrng>,
	gen_ai_system: DefaultedUnknown<RichStrng>,
	gen_ai_request_model: DefaultedUnknown<RichStrng>,
	gen_ai_response_model: DefaultedUnknown<RichStrng>,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct GenAILabelsTokenUsage {
	#[flatten]
	common: GenAILabels,
	gen_ai_token_type: DefaultedUnknown<RichStrng>,
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct TCPLabels {
	pub bind: DefaultedUnknown<RichStrng>,
	pub gateway: DefaultedUnknown<RichStrng>,
	pub listener: DefaultedUnknown<RichStrng>,
	pub protocol: BindProtocol,
}

type Counter = Family<HTTPLabels, prometheus_client::metrics::counter::Counter>;
type Histogram<T> = Family<T, prometheus_client::metrics::histogram::Histogram>;
type TCPCounter = Family<TCPLabels, prometheus_client::metrics::counter::Counter>;

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct BuildLabel {
	tag: &'static str,
}

#[derive(Debug)]
pub struct Metrics {
	pub requests: Counter,
	pub downstream_connection: TCPCounter,

	pub token_usage: Histogram<GenAILabelsTokenUsage>,
}

impl Metrics {
	pub fn new(registry: &mut Registry) -> Self {
		registry.register(
			"build",
			"Agentgateway build information",
			Info::new(BuildLabel {
				tag: version::BuildInfo::new().version,
			}),
		);
		Metrics {
			requests: build(
				registry,
				"requests",
				"The total number of HTTP requests sent",
			),
			downstream_connection: build(
				registry,
				"downstream_connections",
				"The total number of downstream connections established",
			),
			token_usage: build(
				registry,
				"gen_ai.client.token.usage",
				"Number of tokens used per request",
			),
		}
	}
}

fn build<T, C>(registry: &mut Registry, name: &str, help: &str) -> Family<T, C>
where
	T: Clone + std::hash::Hash + Eq + Send + Sync + Debug + EncodeLabelSet + 'static,
	C: Default + Sync + Send + EncodeMetric + prometheus_client::metrics::TypedMetric + Debug+ 'static,
{
	let m = Family::<T, C>::default();
	registry.register(name, help, m.clone());
	m
}
