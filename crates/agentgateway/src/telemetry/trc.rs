use std::ops::Sub;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use http::Version;
use opentelemetry::trace::{Span, SpanContext, SpanKind, TraceState, Tracer as _, TracerProvider};
use opentelemetry::{Key, KeyValue, TraceFlags};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tokio::io::AsyncWriteExt;
pub use traceparent::TraceParent;

use crate::http::Request;
use crate::telemetry::log::RequestLog;

#[derive(Clone, Debug)]
pub struct Tracer {
	pub tracer: Arc<opentelemetry_sdk::trace::SdkTracer>,
	pub provider: SdkTracerProvider,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Config {
	pub endpoint: Option<String>,
}

mod semconv {
	use opentelemetry::Key;

	pub static PROTOCOL_VERSION: Key = Key::from_static_str("network.protocol.version");
	pub static HTTP_ROUTE: Key = Key::from_static_str("http.route");
	pub static REQUEST_METHOD: Key = Key::from_static_str("http.request.method");
	pub static STATUS_CODE: Key = Key::from_static_str("http.response.status_code");
	pub static SERVER_PORT: Key = Key::from_static_str("server.port");
	pub static URL_PATH: Key = Key::from_static_str("url.path");
	pub static URL_SCHEME: Key = Key::from_static_str("url.scheme");
	pub static URL_QUERY: Key = Key::from_static_str("url.query");
	pub static USER_AGENT: Key = Key::from_static_str("user_agent.original");
	pub static PEER_ADDRESS: Key = Key::from_static_str("network.peer.address");
}

impl Tracer {
	pub fn new(cfg: &Config) -> anyhow::Result<Option<Tracer>> {
		let Some(ep) = &cfg.endpoint else {
			return Ok(None);
		};
		let result = opentelemetry_sdk::trace::SdkTracerProvider::builder()
			.with_resource(
				Resource::builder()
					.with_service_name("agentgateway")
					.with_attribute(KeyValue::new(
						"service.version",
						agent_core::version::BuildInfo::new().version,
					))
					.build(),
			)
			.with_batch_exporter(
				opentelemetry_otlp::SpanExporter::builder()
					.with_tonic()
					.with_endpoint(ep)
					.build()?, //
			)
			.build();
		let tracer = result.tracer("agentgateway");
		Ok(Some(Tracer {
			tracer: Arc::new(tracer),
			provider: result,
		}))
	}

	pub fn shutdown(&self) {
		self.provider.shutdown();
	}

	pub fn send(&self, request: &RequestLog) {
		let out_span = request.outgoing_span.as_ref().unwrap();
		if !out_span.is_sampled() {
			return;
		}
		let tcp_info = request.tcp_info.as_ref().expect("tODO");
		let end = SystemTime::now();
		let elapsed = tcp_info.start.elapsed();
		let mut attributes = Vec::with_capacity(7);

		if let Some(method) = &request.method {
			attributes.push(KeyValue::new(
				semconv::REQUEST_METHOD.clone(),
				method.to_string(),
			));
		}
		// For now we only accept HTTP(?)
		attributes.push(KeyValue::new(semconv::URL_SCHEME.clone(), "http"));
		if let Some(code) = &request.status {
			attributes.push(KeyValue::new(
				semconv::STATUS_CODE.clone(),
				code.as_u16() as i64,
			));
		}
		if let Some(path) = &request.path {
			attributes.push(KeyValue::new(semconv::URL_PATH.clone(), path.to_string()));
		}
		match &request.version {
			Some(Version::HTTP_11) => {
				attributes.push(KeyValue::new(semconv::PROTOCOL_VERSION.clone(), "1.1"));
			},
			Some(Version::HTTP_2) => {
				attributes.push(KeyValue::new(semconv::PROTOCOL_VERSION.clone(), "2"));
			},
			_ => {},
		}
		let span_name = match (&request.method, &request.path) {
			(Some(method), Some(path)) => {
				// TODO: should be path match, not the path!
				format!("{method} {path}")
			},
			_ => "unknown".to_string(),
		};
		let out_span = request.outgoing_span.as_ref().unwrap();
		let mut sb = self
			.tracer
			.span_builder(span_name)
			.with_start_time(end.sub(elapsed))
			.with_end_time(SystemTime::now())
			.with_kind(SpanKind::Server)
			.with_attributes(attributes)
			.with_trace_id(out_span.trace_id.into())
			.with_span_id(out_span.span_id.into());

		if let Some(in_span) = &request.incoming_span {
			let parent = SpanContext::new(
				in_span.trace_id.into(),
				in_span.span_id.into(),
				TraceFlags::new(in_span.flags),
				true,
				TraceState::default(),
			);
			sb = sb.with_links(vec![opentelemetry::trace::Link::new(
				parent.clone(),
				vec![],
				0,
			)]);
		}
		sb.start(self.tracer.as_ref()).end()
	}
}

mod traceparent {
	use std::fmt;

	use opentelemetry::TraceFlags;
	use rand::Rng;

	use crate::http::Request;

	/// Represents a traceparent, as defined by https://www.w3.org/TR/trace-context/
	#[derive(Clone, Eq, PartialEq)]
	pub struct TraceParent {
		pub version: u8,
		pub trace_id: u128,
		pub span_id: u64,
		pub flags: u8,
	}

	pub const TRACEPARENT_HEADER: &str = "traceparent";

	impl Default for TraceParent {
		fn default() -> Self {
			Self::new()
		}
	}

	impl TraceParent {
		pub fn new() -> Self {
			let mut rng = rand::rng();
			Self {
				version: 0,
				trace_id: rng.random(),
				span_id: rng.random(),
				flags: 0,
			}
		}
		pub fn insert_header(&self, req: &mut Request) {
			let hv = hyper::header::HeaderValue::from_bytes(format!("{self:?}").as_bytes()).unwrap();
			req.headers_mut().insert(TRACEPARENT_HEADER, hv);
		}
		pub fn from_request(req: &Request) -> Option<Self> {
			req
				.headers()
				.get(TRACEPARENT_HEADER)
				.and_then(|b| b.to_str().ok())
				.and_then(|b| TraceParent::try_from(b).ok())
		}
		pub fn new_span(&self) -> Self {
			let mut rng = rand::rng();
			let mut cpy: TraceParent = self.clone();
			cpy.span_id = rng.random();
			cpy
		}
		pub fn trace_id(&self) -> String {
			format!("{:032x}", self.trace_id)
		}
		pub fn span_id(&self) -> String {
			format!("{:016x}", self.span_id)
		}
		pub fn is_sampled(&self) -> bool {
			(self.flags & 0x01) == 0x01
		}
	}

	impl fmt::Debug for TraceParent {
		fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
			write!(
				f,
				"{:02x}-{:032x}-{:016x}-{:02x}",
				self.version, self.trace_id, self.span_id, self.flags
			)
		}
	}

	impl fmt::Display for TraceParent {
		fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
			write!(f, "{:032x}", self.trace_id,)
		}
	}

	impl TryFrom<&str> for TraceParent {
		type Error = anyhow::Error;

		fn try_from(value: &str) -> Result<Self, Self::Error> {
			if value.len() != 55 {
				anyhow::bail!("traceparent malformed length was {}", value.len())
			}

			let segs: Vec<&str> = value.split('-').collect();

			Ok(Self {
				version: u8::from_str_radix(segs[0], 16)?,
				trace_id: u128::from_str_radix(segs[1], 16)?,
				span_id: u64::from_str_radix(segs[2], 16)?,
				flags: u8::from_str_radix(segs[3], 16)?,
			})
		}
	}
}
