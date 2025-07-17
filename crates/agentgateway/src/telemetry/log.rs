use std::collections::{BTreeMap, HashSet};
use std::fmt::Debug;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::{Instant, SystemTime};

use crate::telemetry::metrics::{HTTPLabels, Metrics};
use crate::telemetry::trc;
use crate::transport::stream::{TCPConnectionInfo, TLSConnectionInfo};
use crate::types::agent::{
	BackendName, BindName, GatewayName, ListenerName, RouteName, RouteRuleName, Target,
};
use crate::types::discovery::NamespacedHostname;
use crate::{cel, llm, mcp};
use agent_core::telemetry::{OptionExt, ValueBag, debug, display};
use crossbeam::atomic::AtomicCell;
use http_body::{Body, Frame, SizeHint};
use tracing::{Level, event, log};

/// AsyncLog is a wrapper around an item that can be atomically set.
/// The intent is to provide additional info to the log after we have lost the RequestLog reference,
/// generally for things that rely on the response body.
#[derive(Clone)]
pub struct AsyncLog<T>(Arc<AtomicCell<Option<T>>>);

impl<T> AsyncLog<T> {
	// non_atomic_mutate is a racey method to modify the current value.
	// If there is no current value, a default is used.
	// This is NOT atomically safe; during the mutation, loads() on the item will be empty.
	// This is ok for our usage cases
	pub fn non_atomic_mutate(&self, f: impl FnOnce(&mut T)) {
		let Some(mut cur) = self.0.take() else {
			return;
		};
		f(&mut cur);
		self.0.store(Some(cur));
	}
}

impl<T> AsyncLog<T> {
	pub fn store(&self, v: Option<T>) {
		self.0.store(v)
	}
	pub fn take(&self) -> Option<T> {
		self.0.take()
	}
}

impl<T: Copy> AsyncLog<T> {
	pub fn load(&self) -> Option<T> {
		self.0.load()
	}
}

impl<T> Default for AsyncLog<T> {
	fn default() -> Self {
		AsyncLog(Arc::new(AtomicCell::new(None)))
	}
}

impl<T: Debug> Debug for AsyncLog<T> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("AsyncLog").finish_non_exhaustive()
	}
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct Config {
	pub filter: Option<Arc<cel::Expression>>,
	pub fields: Arc<LoggingFields>,
}

#[derive(serde::Serialize, Default, Clone, Debug)]
pub struct LoggingFields {
	pub remove: HashSet<String>,
	pub add: BTreeMap<String, Arc<cel::Expression>>,
}

impl LoggingFields {
	pub fn has(&self, k: &str) -> bool {
		self.remove.contains(k) || self.add.contains_key(k)
	}
}

#[derive(Default, Debug)]
pub struct RequestLog {
	pub filter: Option<cel::ExpressionCall>,
	pub fields: Option<Arc<LoggingFields>>,

	pub tracer: Option<trc::Tracer>,
	pub metrics: Option<Arc<Metrics>>,

	pub start: Option<Instant>,
	pub tcp_info: Option<TCPConnectionInfo>,
	pub tls_info: Option<TLSConnectionInfo>,

	pub endpoint: Option<Target>,

	pub bind_name: Option<BindName>,
	pub gateway_name: Option<GatewayName>,
	pub listener_name: Option<ListenerName>,
	pub route_rule_name: Option<RouteRuleName>,
	pub route_name: Option<RouteName>,
	pub backend_name: Option<BackendName>,

	pub host: Option<String>,
	pub method: Option<::http::Method>,
	pub path: Option<String>,
	pub version: Option<::http::Version>,
	pub status: Option<crate::http::StatusCode>,

	pub jwt_sub: Option<String>,

	pub retry_attempt: Option<u8>,
	pub error: Option<String>,

	pub grpc_status: AsyncLog<u8>,
	pub mcp_status: AsyncLog<mcp::sse::MCPInfo>,

	pub incoming_span: Option<trc::TraceParent>,
	pub outgoing_span: Option<trc::TraceParent>,

	pub llm_request: Option<llm::LLMRequest>,
	pub llm_response: AsyncLog<llm::LLMResponse>,

	pub a2a_method: Option<&'static str>,

	pub inference_pool: Option<SocketAddr>,
}

impl Drop for RequestLog {
	fn drop(&mut self) {
		if let Some(t) = &self.tracer {
			t.send(self)
		};
		if let Some(m) = &self.metrics {
			m.requests
				.get_or_create(&HTTPLabels {
					bind: (&self.bind_name).into(),
					gateway: (&self.gateway_name).into(),
					listener: (&self.listener_name).into(),
					route: (&self.route_name).into(),
					route_rule: (&self.route_rule_name).into(),
					backend: (&self.backend_name).into(),
					method: self.method.clone().into(),
					status: self.status.as_ref().map(|s| s.as_u16()).into(),
				})
				.inc();
		}
		if !tracing::enabled!(target: "request", Level::INFO) {
			return;
		}
		if let Some(filter) = &self.filter {
			if !filter.eval_bool() {
				return;
			}
		}
		let tcp_info = self.tcp_info.as_ref().expect("tODO");

		let dur = format!("{}ms", self.start.unwrap().elapsed().as_millis());
		let grpc = self.grpc_status.load();

		let llm_response = self.llm_response.take();
		if let (Some(req), Some(resp)) = (self.llm_request.as_ref(), llm_response.as_ref()) {
			if Some(req.input_tokens) != resp.input_tokens_from_response {
				// TODO: remove this, just for dev
				tracing::warn!("maybe bug: mismatch in tokens {req:?}, {resp:?}");
			}
		}
		let input_tokens = llm_response
			.as_ref()
			.and_then(|t| t.input_tokens_from_response)
			.or_else(|| self.llm_request.as_ref().map(|req| req.input_tokens));

		let mcp = self.mcp_status.take();

		let trace_id = self.outgoing_span.as_ref().map(|id| id.trace_id());
		let span_id = self.outgoing_span.as_ref().map(|id| id.span_id());

		let fields = self.fields.take().unwrap_or_default();

		macro_rules! log {
			($key:expr, $value:expr$(,)?) => {
				($key, if fields.has($key) { None } else { $value })
			};
		}

		let mut kv = vec![
			log!("gateway", self.gateway_name.display()),
			log!("listener", self.listener_name.display()),
			log!("route_rule", self.route_rule_name.display()),
			log!("route", self.route_name.display()),
			log!("endpoint", self.endpoint.display()),
			log!("src.addr", Some(display(&tcp_info.peer_addr))),
			log!("http.method", self.method.display()),
			log!("http.host", self.host.display()),
			log!("http.path", self.path.display()),
			// TODO: incoming vs outgoing
			log!("http.version", self.version.as_ref().map(debug)),
			log!(
				"http.status",
				self.status.as_ref().map(|s| s.as_u16().into()),
			),
			log!("grpc.status", grpc.map(Into::into)),
			log!("trace.id", trace_id.display()),
			log!("span.id", span_id.display()),
			log!("jwt.sub", self.jwt_sub.display()),
			log!("a2a.method", self.a2a_method.display()),
			log!(
				"mcp.target",
				mcp
					.as_ref()
					.and_then(|m| m.target_name.as_ref())
					.map(display),
			),
			log!(
				"mcp.tool",
				mcp
					.as_ref()
					.and_then(|m| m.tool_call_name.as_ref())
					.map(display),
			),
			log!(
				"inferencepool.selected_endpoint",
				self.inference_pool.display(),
			),
			log!(
				"llm.provider",
				self.llm_request.as_ref().map(|l| display(&l.provider)),
			),
			log!(
				"llm.request.model",
				self.llm_request.as_ref().map(|l| display(&l.request_model)),
			),
			log!("llm.request.tokens", input_tokens.map(Into::into)),
			log!(
				"llm.response.model",
				llm_response
					.as_ref()
					.and_then(|l| l.provider_model.display())
			),
			log!(
				"llm.response.tokens",
				llm_response
					.as_ref()
					.and_then(|l| l.output_tokens)
					.map(Into::into),
			),
			log!("retry.attempt", self.retry_attempt.display()),
			log!("error", self.error.display()),
			log!("duration", Some(dur.as_str().into())),
		];
		kv.reserve(fields.add.len());

		// To avoid lifetime issues need to store the expression before we give it to ValueBag reference.
		// TODO: we could allow log() to take a list of borrows and then a list of OwnedValueBag
		let mut raws = Vec::with_capacity(fields.add.len());
		for (k, v) in &fields.add {
			let celv = cel::ExpressionCall::from_expression(v.clone())
				.eval()
				.ok()
				.and_then(|v| v.json().ok());
			raws.push((k, celv));
		}
		for (k, v) in &raws {
			// TODO: convert directly instead of via json()
			let eval = v.as_ref().map(|v| ValueBag::capture_serde1(v));
			kv.push((k, eval));
		}

		agent_core::telemetry::log("info", "request", &kv);
	}
}

fn to_value<T: AsRef<str>>(t: &T) -> impl tracing::Value + '_ {
	let v: &str = t.as_ref();
	v
}

pin_project_lite::pin_project! {
		/// A data stream created from a [`Body`].
		#[derive(Debug)]
		pub struct LogBody<B> {
				#[pin]
				body: B,
				log: RequestLog,
		}
}

impl<B> LogBody<B> {
	/// Create a new `LogBody`
	pub fn new(body: B, log: RequestLog) -> Self {
		Self { body, log }
	}
}

impl<B: Body + Debug> Body for LogBody<B>
where
	B::Data: Debug,
{
	type Data = B::Data;
	type Error = B::Error;

	fn poll_frame(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
		let this = self.project();
		let result = ready!(this.body.poll_frame(cx));
		match result {
			Some(Ok(frame)) => {
				if let Some(trailer) = frame.trailers_ref() {
					crate::proxy::httpproxy::maybe_set_grpc_status(&this.log.grpc_status, trailer);
				}
				Poll::Ready(Some(Ok(frame)))
			},
			res => Poll::Ready(res),
		}
	}

	fn is_end_stream(&self) -> bool {
		self.body.is_end_stream()
	}

	fn size_hint(&self) -> SizeHint {
		self.body.size_hint()
	}
}
