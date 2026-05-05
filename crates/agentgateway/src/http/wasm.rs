use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use macro_rules_attribute::apply;
use wasmtime::component::{Accessor, Component, HasSelf, Linker};
use wasmtime::{Config, Engine as WasmtimeEngine, Store};

use crate::cel::RequestSnapshot;
use crate::http::{Body, Request};
use crate::proxy::httpproxy::PolicyClient;
use crate::proxy::{ProxyError, ProxyResponse};
use crate::types::agent::{ResourceName, SimpleBackend, SimpleBackendWithPolicies, Target};
use crate::*;

wasmtime::component::bindgen!({
	world: "request-policy",
	path: "wit",
	imports: { default: async | store | trappable },
	exports: { default: async | trappable },
});

const MAX_IDLE_INSTANCES: usize = 64;

#[derive(thiserror::Error, Debug)]
pub enum Error {
	#[error("wasm policy failed: {0}")]
	Trap(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct Wasm {
	engine: Arc<WasmtimeEngine>,
	pre: RequestPolicyPre<WasmState>,
	cel_cache: CelExpressionCache,
	instances: Arc<tokio::sync::Mutex<Vec<WasmInstance>>>,
}

impl std::fmt::Debug for Wasm {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Wasm").finish_non_exhaustive()
	}
}

impl serde::Serialize for Wasm {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str("<wasm>")
	}
}

struct WasmState {
	client: PolicyClient,
	req: RequestSnapshot,
	cel_cache: CelExpressionCache,
}

type CelExpressionCache = Arc<Mutex<HashMap<String, Arc<cel::Expression>>>>;

struct WasmInstance {
	store: Store<WasmState>,
	instance: RequestPolicy,
}

impl Wasm {
	pub fn new(bytes: &[u8]) -> anyhow::Result<Self> {
		let mut config = Config::new();
		config.wasm_component_model(true);
		config.wasm_component_model_async(true);
		config.concurrency_support(true);
		let engine = Arc::new(WasmtimeEngine::new(&config)?);
		let component = Component::new(&engine, bytes)?;
		let mut linker = Linker::new(&engine);
		RequestPolicy::add_to_linker::<_, HasSelf<WasmState>>(&mut linker, |state| state)?;
		let pre = RequestPolicyPre::new(linker.instantiate_pre(&component)?)?;
		Ok(Self {
			engine,
			pre,
			cel_cache: Default::default(),
			instances: Default::default(),
		})
	}

	async fn run(&self, client: &PolicyClient, req: &mut Request) -> Result<bool, Error> {
		let policy_req = policy_request_from(req);
		let state = WasmState {
			client: client.clone(),
			req: cel::snapshot_request(req, false),
			cel_cache: self.cel_cache.clone(),
		};
		let mut instance = self.instance(state).await?;
		let allowed = instance.call(policy_req).await;
		if allowed.is_ok() {
			instance.store.data_mut().req = empty_request_snapshot();
			let mut instances = self.instances.lock().await;
			if instances.len() < MAX_IDLE_INSTANCES {
				instances.push(instance);
			}
		}
		let allowed = allowed?;
		Ok(allowed)
	}

	async fn instance(&self, state: WasmState) -> Result<WasmInstance, Error> {
		if let Some(mut instance) = self.instances.lock().await.pop() {
			*instance.store.data_mut() = state;
			return Ok(instance);
		}
		let mut store = Store::new(&self.engine, state);
		let instance = self
			.pre
			.instantiate_async(&mut store)
			.await
			.map_err(wasmtime_error)?;
		Ok(WasmInstance { store, instance })
	}
}

impl WasmInstance {
	async fn call(
		&mut self,
		policy_req: exports::agentgateway::policy::policy::Request,
	) -> Result<bool, Error> {
		let policy = self.instance.agentgateway_policy_policy();
		let allowed = self
			.store
			.run_concurrent(async |accessor| {
				policy
					.call_apply(accessor, policy_req)
					.await
					.map_err(wasmtime_error)?
					.map_err(|err| Error::Trap(anyhow::Error::msg(err)))
			})
			.await
			.map_err(wasmtime_error)??;
		Ok(allowed)
	}
}

impl crate::store::RequestPolicyTrait for Wasm {
	async fn apply(
		&self,
		client: &PolicyClient,
		_log: &mut crate::telemetry::log::RequestLog,
		req: &mut Request,
	) -> Result<crate::http::PolicyResponse, ProxyResponse> {
		match self.run(client, req).await {
			Ok(true) => Ok(crate::http::PolicyResponse::default()),
			Ok(false) => Err(ProxyResponse::from(ProxyError::AuthorizationFailed)),
			Err(err) => Err(ProxyResponse::from(ProxyError::Processing(err.into()))),
		}
	}
}

impl agentgateway::policy::host::Host for WasmState {}

impl agentgateway::policy::host::HostWithStore for HasSelf<WasmState> {
	fn http_call<T: Send>(
		accessor: &Accessor<T, Self>,
		url: String,
	) -> impl Future<Output = wasmtime::Result<Result<String, String>>> + Send {
		let client = accessor.with(|mut access| access.get().client.clone());
		async move {
			Ok(
				host_http_call(&client, &url)
					.await
					.map_err(|err| err.to_string()),
			)
		}
	}

	fn cel_eval_bool<T: Send>(
		accessor: &Accessor<T, Self>,
		expr: String,
	) -> impl Future<Output = wasmtime::Result<Result<bool, String>>> + Send {
		let res = accessor.with(|mut access| {
			let state = access.get();
			host_cel_eval_bool(&state.req, &state.cel_cache, &expr).map_err(|err| err.to_string())
		});
		async move { Ok(res) }
	}
}

fn policy_request_from(req: &Request) -> exports::agentgateway::policy::policy::Request {
	exports::agentgateway::policy::policy::Request {
		method: req.method().to_string(),
		path_with_query: req
			.uri()
			.path_and_query()
			.map(|path| path.as_str())
			.unwrap_or("/")
			.to_string(),
		headers: req
			.headers()
			.iter()
			.map(|(name, value)| (name.as_str().to_string(), value.as_bytes().to_vec()))
			.collect(),
	}
}

fn empty_request_snapshot() -> RequestSnapshot {
	RequestSnapshot {
		method: ::http::Method::GET,
		path: ::http::Uri::from_static("/"),
		host: None,
		scheme: None,
		version: ::http::Version::HTTP_11,
		headers: ::http::HeaderMap::new(),
		body: None,
		recorded_body: None,
		jwt: None,
		api_key: None,
		basic_auth: None,
		backend: None,
		source: None,
		start_time: None,
		extauthz: None,
		extproc: None,
		metadata: None,
		llm: None,
	}
}

async fn host_http_call(client: &PolicyClient, url: &str) -> anyhow::Result<String> {
	let uri: ::http::Uri = url.parse()?;
	let backend = backend_from_uri(uri.clone())?;
	let req = ::http::Request::builder()
		.method(::http::Method::GET)
		.uri(uri)
		.body(Body::empty())?;
	let resp = client.call(req, backend).await?;
	if resp.status() != crate::http::StatusCode::OK {
		anyhow::bail!("http_call returned {}", resp.status());
	}
	let body = crate::http::read_resp_body(resp).await?;
	Ok(String::from_utf8(body.to_vec())?)
}

fn backend_from_uri(uri: ::http::Uri) -> anyhow::Result<SimpleBackendWithPolicies> {
	let scheme = uri.scheme_str().unwrap_or("http");
	if scheme != "http" {
		anyhow::bail!("wasm http_call supports http URLs only");
	}
	let authority = uri
		.authority()
		.ok_or_else(|| anyhow::anyhow!("wasm http_call URL must include an authority"))?;
	let host = authority
		.host()
		.trim_start_matches('[')
		.trim_end_matches(']')
		.to_string();
	let port = authority.port_u16().unwrap_or(80);
	let target = Target::from((host.as_str(), port));
	Ok(SimpleBackendWithPolicies {
		backend: SimpleBackend::Opaque(
			ResourceName::new(strng::format!("wasm-http-call-{host}-{port}"), strng::EMPTY),
			target,
		),
		inline_policies: Vec::new(),
	})
}

fn host_cel_eval_bool(
	req: &RequestSnapshot,
	cache: &CelExpressionCache,
	expr: &str,
) -> anyhow::Result<bool> {
	let expr = cached_cel_expression(cache, expr)?;
	let exec = cel::Executor::new_request_snapshot(req);
	Ok(exec.eval_bool(expr.as_ref()))
}

fn cached_cel_expression(
	cache: &CelExpressionCache,
	expr: &str,
) -> anyhow::Result<Arc<cel::Expression>> {
	if let Some(expr) = cache
		.lock()
		.map_err(|_| anyhow::anyhow!("wasm CEL cache lock poisoned"))?
		.get(expr)
		.cloned()
	{
		return Ok(expr);
	}

	let key = expr.to_string();
	let compiled = Arc::new(cel::Expression::new_strict(expr)?);
	let mut cache = cache
		.lock()
		.map_err(|_| anyhow::anyhow!("wasm CEL cache lock poisoned"))?;
	Ok(cache.entry(key).or_insert(compiled).clone())
}

#[apply(schema_de!)]
pub struct LocalWasm {
	/// Path to a WebAssembly component.
	#[serde(default)]
	pub file: Option<PathBuf>,
	/// Base64-encoded WebAssembly component bytes.
	#[serde(default)]
	pub inline: Option<String>,
}

fn wasmtime_error(err: wasmtime::Error) -> Error {
	Error::Trap(anyhow::Error::msg(err.to_string()))
}

impl LocalWasm {
	pub fn try_into_policy(self) -> anyhow::Result<Wasm> {
		let bytes = match (self.file, self.inline) {
			(Some(file), None) => fs_err::read(file)?,
			(None, Some(inline)) => base64::engine::general_purpose::STANDARD.decode(inline)?,
			(Some(_), Some(_)) => anyhow::bail!("wasm policy must set only one of file or inline"),
			(None, None) => anyhow::bail!("wasm policy requires file or inline"),
		};
		Wasm::new(&bytes)
	}
}
