use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use macro_rules_attribute::apply;
use wasmtime::{Caller, Engine as WasmtimeEngine, Linker, Module, Store};

use crate::cel::RequestSnapshot;
use crate::http::{Body, Request, StatusCode};
use crate::proxy::httpproxy::PolicyClient;
use crate::proxy::{ProxyError, ProxyResponse};
use crate::types::agent::{ResourceName, SimpleBackend, SimpleBackendWithPolicies, Target};
use crate::*;

const HOST_MODULE: &str = "agentgateway:policy/host";
#[derive(thiserror::Error, Debug)]
pub enum Error {
	#[error("wasm policy failed: {0}")]
	Trap(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct Wasm {
	engine: Arc<WasmtimeEngine>,
	module: Arc<Module>,
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
}

impl Wasm {
	pub fn new(bytes: &[u8]) -> anyhow::Result<Self> {
		let engine = Arc::new(WasmtimeEngine::default());
		let module = Arc::new(Module::new(&engine, bytes)?);
		Ok(Self { engine, module })
	}

	async fn run(&self, client: &PolicyClient, req: &mut Request) -> Result<i32, Error> {
		let mut linker = Linker::new(&self.engine);

		linker
			.func_wrap_async(
				HOST_MODULE,
				"http_get",
				|caller: Caller<'_, WasmState>,
				 (url_ptr, url_len, out_ptr, out_cap): (i32, i32, i32, i32)| {
					Box::new(async move {
						host_http_get(caller, url_ptr, url_len, out_ptr, out_cap)
							.await
							.unwrap_or(-1)
					})
				},
			)
			.map_err(wasmtime_error)?;
		linker
			.func_wrap_async(
				HOST_MODULE,
				"cel_eval_bool",
				|caller: Caller<'_, WasmState>, (expr_ptr, expr_len): (i32, i32)| {
					Box::new(async move { host_cel_eval_bool(caller, expr_ptr, expr_len).unwrap_or(-1) })
				},
			)
			.map_err(wasmtime_error)?;

		let mut store = Store::new(
			&self.engine,
			WasmState {
				client: client.clone(),
				req: cel::snapshot_request(req, false),
			},
		);
		let instance = linker
			.instantiate_async(&mut store, &self.module)
			.await
			.map_err(wasmtime_error)?;
		let apply = instance
			.get_typed_func::<(), i32>(&mut store, "policy_apply")
			.map_err(wasmtime_error)?;
		Ok(
			apply
				.call_async(&mut store, ())
				.await
				.map_err(wasmtime_error)?,
		)
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
			Ok(0) => Ok(crate::http::PolicyResponse::default()),
			Ok(1) => Err(ProxyResponse::from(ProxyError::AuthorizationFailed)),
			Ok(code) => Err(ProxyResponse::from(ProxyError::ProcessingString(format!(
				"wasm policy returned invalid status code {code}"
			)))),
			Err(err) => Err(ProxyResponse::from(ProxyError::Processing(err.into()))),
		}
	}
}

async fn host_http_get(
	mut caller: Caller<'_, WasmState>,
	url_ptr: i32,
	url_len: i32,
	out_ptr: i32,
	out_cap: i32,
) -> anyhow::Result<i32> {
	let url = read_guest_string(&mut caller, url_ptr, url_len)?;
	let uri: http::Uri = url.parse()?;
	let scheme = uri.scheme_str().unwrap_or("http");
	if scheme != "http" {
		anyhow::bail!("wasm http_get supports http URLs only");
	}
	let authority = uri
		.authority()
		.ok_or_else(|| anyhow::anyhow!("wasm http_get URL must include an authority"))?;
	let host = authority
		.host()
		.trim_start_matches('[')
		.trim_end_matches(']')
		.to_string();
	let port = authority.port_u16().unwrap_or(80);
	let target = Target::from((host.as_str(), port));
	let backend = SimpleBackendWithPolicies {
		backend: SimpleBackend::Opaque(
			ResourceName::new(strng::format!("wasm-http-get-{host}-{port}"), strng::EMPTY),
			target,
		),
		inline_policies: Vec::new(),
	};
	let req = ::http::Request::builder()
		.method(http::Method::GET)
		.uri(uri)
		.body(Body::empty())?;
	let resp = caller.data().client.call(req, backend).await?;
	let status = resp.status();
	if status != StatusCode::OK {
		return Ok(-(status.as_u16() as i32));
	}
	let body = crate::http::read_resp_body(resp).await?;
	write_guest_bytes(&mut caller, out_ptr, out_cap, &body)
}

fn host_cel_eval_bool(
	mut caller: Caller<'_, WasmState>,
	expr_ptr: i32,
	expr_len: i32,
) -> anyhow::Result<i32> {
	let expr = read_guest_string(&mut caller, expr_ptr, expr_len)?;
	let expr = cel::Expression::new_strict(&expr)?;
	let exec = cel::Executor::new_request_snapshot(&caller.data().req);
	Ok(i32::from(exec.eval_bool(&expr)))
}

fn read_guest_string(
	caller: &mut Caller<'_, WasmState>,
	ptr: i32,
	len: i32,
) -> anyhow::Result<String> {
	let bytes = read_guest_bytes(caller, ptr, len)?;
	Ok(std::str::from_utf8(bytes)?.to_string())
}

fn read_guest_bytes<'a>(
	caller: &'a mut Caller<'_, WasmState>,
	ptr: i32,
	len: i32,
) -> anyhow::Result<&'a [u8]> {
	let memory = caller
		.get_export("memory")
		.and_then(|export| export.into_memory())
		.ok_or_else(|| anyhow::anyhow!("wasm policy must export memory"))?;
	let ptr = usize::try_from(ptr)?;
	let len = usize::try_from(len)?;
	memory
		.data(caller)
		.get(ptr..ptr.saturating_add(len))
		.ok_or_else(|| anyhow::anyhow!("wasm policy passed an invalid memory range"))
}

fn write_guest_bytes(
	caller: &mut Caller<'_, WasmState>,
	ptr: i32,
	cap: i32,
	bytes: &[u8],
) -> anyhow::Result<i32> {
	let memory = caller
		.get_export("memory")
		.and_then(|export| export.into_memory())
		.ok_or_else(|| anyhow::anyhow!("wasm policy must export memory"))?;
	let ptr = usize::try_from(ptr)?;
	let cap = usize::try_from(cap)?;
	if bytes.len() > cap {
		return Ok(-2);
	}
	let dst = memory
		.data_mut(caller)
		.get_mut(ptr..ptr.saturating_add(bytes.len()))
		.ok_or_else(|| anyhow::anyhow!("wasm policy passed an invalid output memory range"))?;
	dst.copy_from_slice(bytes);
	Ok(i32::try_from(bytes.len())?)
}

#[apply(schema_de!)]
pub struct LocalWasm {
	/// Path to a core WebAssembly module.
	#[serde(default)]
	pub file: Option<PathBuf>,
	/// Base64-encoded core WebAssembly module bytes.
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
