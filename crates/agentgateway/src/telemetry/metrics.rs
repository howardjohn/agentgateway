use agent_core::metrics::{DefaultedUnknown, EncodeDisplay};
use agent_core::strng::RichStrng;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct CommonTrafficLabels {
	pub gateway: DefaultedUnknown<RichStrng>,
	pub listener: DefaultedUnknown<RichStrng>,
	pub route: DefaultedUnknown<RichStrng>,
	pub route_rule: DefaultedUnknown<RichStrng>,
	pub backend: DefaultedUnknown<RichStrng>,

	pub method: DefaultedUnknown<EncodeDisplay<http::Method>>,
	pub status: DefaultedUnknown<EncodeDisplay<u16>>,
}

type Counter = Family<CommonTrafficLabels, prometheus_client::metrics::counter::Counter>;

#[derive(Debug)]
pub struct Metrics {
	pub requests: Counter,
}

impl Metrics {
	pub fn new(registry: &mut Registry) -> Self {
		let mut build = |name: &str, help: &str| {
			let m = Family::default();
			registry.register(name, help, m.clone());
			m
		};
		Metrics {
			requests: build("requests", "The total number of HTTP requests sent"),
		}
	}
}
