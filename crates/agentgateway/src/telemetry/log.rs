use std::collections::{BTreeMap, HashSet};
use std::fmt::Debug;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, ready};
use std::time::{Instant, SystemTime};

use agent_core::telemetry::{OptionExt, ValueBag, debug, display};
use crossbeam::atomic::AtomicCell;
use frozen_collections::{FzHashSet, FzOrderedMap};
use http_body::{Body, Frame, SizeHint};
use serde_json::Value;
use tracing::{Level, event, log};

use crate::cel::{ContextBuilder, Expression};
use crate::telemetry::metrics::{HTTPLabels, Metrics};
use crate::telemetry::trc;
use crate::transport::stream::{TCPConnectionInfo, TLSConnectionInfo};
use crate::types::agent::{
	BackendName, BindName, GatewayName, ListenerName, RouteName, RouteRuleName, Target,
};
use crate::types::discovery::NamespacedHostname;
use crate::{cel, llm, mcp};

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

#[derive(serde::Serialize, Debug, Clone)]
pub struct Config {
	pub filter: Option<Arc<cel::Expression>>,
	pub fields: Arc<LoggingFields>,
}

#[derive(serde::Serialize, Default, Clone, Debug)]
pub struct LoggingFields {
	pub remove: FzHashSet<String>,
	pub add: FzOrderedMap<String, Arc<cel::Expression>>,
}

impl LoggingFields {
	pub fn has(&self, k: &str) -> bool {
		self.remove.contains(k) || self.add.contains_key(k)
	}
}

#[derive(Debug)]
pub struct CelLogging {
	pub cel_context: cel::ContextBuilder,
	pub filter: Option<Arc<cel::Expression>>,
	pub fields: Arc<LoggingFields>,
}

pub struct CelLoggingExecutor<'a> {
	pub executor: cel::Executor<'a>,
	pub filter: &'a Option<Arc<cel::Expression>>,
	pub fields: &'a Arc<LoggingFields>,
}

impl<'a> CelLoggingExecutor<'a> {
	fn eval_filter(&self) -> bool {
		match self.filter.as_deref() {
			Some(f) => self.executor.eval_bool(f),
			None => true,
		}
	}

	fn eval_additions(&self) -> Vec<(&String, Option<Value>)> {
		let mut raws = Vec::with_capacity(self.fields.add.len());
		for (k, v) in &self.fields.add {
			let celv = self
				.executor
				.eval(v.as_ref())
				.ok()
				.filter(|v| !matches!(v, cel_interpreter::Value::Null))
				.and_then(|v| v.json().ok());

			raws.push((k, celv));
		}
		raws
	}
}

impl CelLogging {
	pub fn new(cfg: Config) -> Self {
		let mut cel_context = cel::ContextBuilder::new();
		if let Some(f) = &cfg.filter {
			cel_context.register_expression(f.as_ref());
		}
		for v in cfg.fields.add.values() {
			cel_context.register_expression(v.as_ref());
		}
		Self {
			cel_context,
			filter: cfg.filter,
			fields: cfg.fields,
		}
	}

	pub fn register(&mut self, fields: &LoggingFields) {
		for v in fields.add.values() {
			self.cel_context.register_expression(v.as_ref());
		}
	}

	pub fn ctx(&mut self) -> &mut ContextBuilder {
		&mut self.cel_context
	}

	pub fn build(&self) -> Result<CelLoggingExecutor, cel::Error> {
		let CelLogging {
			cel_context,
			filter,
			fields,
		} = self;
		let executor = cel_context.build()?;
		Ok(CelLoggingExecutor {
			executor,
			filter,
			fields,
		})
	}
}

pub struct DropOnLog {
	log: Option<RequestLog>
}

#[derive(Default, Debug)]
pub struct RequestLog {
	pub cel: Option<CelLogging>,

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

impl Drop for DropOnLog {
	fn drop(&mut self) {
		let Some(mut log) = self.log.take() else {
			return
		};
		if let Some(m) = &log.metrics {
			m.requests
				.get_or_create(&HTTPLabels {
					bind: (&log.bind_name).into(),
					gateway: (&log.gateway_name).into(),
					listener: (&log.listener_name).into(),
					route: (&log.route_name).into(),
					route_rule: (&log.route_rule_name).into(),
					backend: (&log.backend_name).into(),
					method: log.method.clone().into(),
					status: log.status.as_ref().map(|s| s.as_u16()).into(),
				})
				.inc();
		}
		let enable_trace = log.tracer.is_some();
		if !tracing::enabled!(target: "request", Level::INFO) && !enable_trace {
			return;
		}
		let cel = log.cel.take().unwrap();
		let Ok(cel_exec) = cel.build() else {
			tracing::warn!("failed to build CEL context");
			return;
		};
		let enable_logs = cel_exec.eval_filter();
		if !enable_logs && !enable_trace {
			return;
		}

		let tcp_info = log.tcp_info.as_ref().expect("tODO");

		let dur = format!("{}ms", log.start.unwrap().elapsed().as_millis());
		let grpc = log.grpc_status.load();

		let llm_response = log.llm_response.take();
		if let (Some(req), Some(resp)) = (log.llm_request.as_ref(), llm_response.as_ref()) {
			if Some(req.input_tokens) != resp.input_tokens_from_response {
				// TODO: remove this, just for dev
				tracing::warn!("maybe bug: mismatch in tokens {req:?}, {resp:?}");
			}
		}
		let input_tokens = llm_response
			.as_ref()
			.and_then(|t| t.input_tokens_from_response)
			.or_else(|| log.llm_request.as_ref().map(|req| req.input_tokens));

		let mcp = log.mcp_status.take();

		let trace_id = log.outgoing_span.as_ref().map(|id| id.trace_id());
		let span_id = log.outgoing_span.as_ref().map(|id| id.span_id());

		let fields = cel_exec.fields.as_ref();

		macro_rules! log {
			($key:expr, $value:expr$(,)?) => {
				($key, if fields.has($key) { None } else { $value })
			};
		}

		let mut kv = vec![
			log!("gateway", log.gateway_name.display()),
			log!("listener", log.listener_name.display()),
			log!("route_rule", log.route_rule_name.display()),
			log!("route", log.route_name.display()),
			log!("endpoint", log.endpoint.display()),
			log!("src.addr", Some(display(&tcp_info.peer_addr))),
			log!("http.method", log.method.display()),
			log!("http.host", log.host.display()),
			log!("http.path", log.path.display()),
			// TODO: incoming vs outgoing
			log!("http.version", log.version.as_ref().map(debug)),
			log!(
				"http.status",
				log.status.as_ref().map(|s| s.as_u16().into()),
			),
			log!("grpc.status", grpc.map(Into::into)),
			log!("trace.id", trace_id.display()),
			log!("span.id", span_id.display()),
			log!("jwt.sub", log.jwt_sub.display()),
			log!("a2a.method", log.a2a_method.display()),
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
				log.inference_pool.display(),
			),
			log!(
				"llm.provider",
				log.llm_request.as_ref().map(|l| display(&l.provider)),
			),
			log!(
				"llm.request.model",
				log.llm_request.as_ref().map(|l| display(&l.request_model)),
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
			log!("retry.attempt", log.retry_attempt.display()),
			log!("error", log.error.display()),
			log!("duration", Some(dur.as_str().into())),
		];
		if enable_trace {
			if let Some(t) = &log.tracer {
				t.send(&log, kv.as_slice())
			};
		}
		if enable_logs {
			kv.reserve(fields.add.len());

			// To avoid lifetime issues need to store the expression before we give it to ValueBag reference.
			// TODO: we could allow log() to take a list of borrows and then a list of OwnedValueBag
			let raws = cel_exec.eval_additions();
			for (k, v) in &raws {
				// TODO: convert directly instead of via json()
				let eval = v.as_ref().map(ValueBag::capture_serde1);
				kv.push((k, eval));
			}

			agent_core::telemetry::log("info", "request", &kv);
		}
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
