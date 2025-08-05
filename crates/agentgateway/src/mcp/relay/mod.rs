use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::fmt::{Debug, Formatter};
use std::hash::BuildHasherDefault;
use std::sync::Arc;
use std::time::Duration;

use agent_core::metrics::Recorder;
use agent_core::prelude::Strng;
use agent_core::trcng;
use http::header::AUTHORIZATION;
use http::request::Parts;
use http::{HeaderMap, HeaderName, HeaderValue};
use itertools::Itertools;
use opentelemetry::global::BoxedSpan;
use opentelemetry::trace::{SpanContext, SpanKind, TraceContextExt, TraceState, Tracer};
use opentelemetry::{Context, TraceFlags};
use rmcp::model::{CallToolRequestParam, Tool, *};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::{RoleClient, RoleServer, ServerHandler, model};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::instrument;

use crate::cel::ContextBuilder;
use crate::http::jwt::Claims;
use crate::mcp::rbac;
use crate::mcp::rbac::{Identity, RuleSets};
use crate::mcp::sse::{MCPInfo, McpBackendGroup};
use crate::proxy::httpproxy::PolicyClient;
use crate::store::Stores;
use crate::telemetry::log::AsyncLog;
use crate::telemetry::trc::TraceParent;
use crate::transport::stream::{TCPConnectionInfo, TLSConnectionInfo};
use crate::types::agent::{McpAuthorization, McpBackend};
use crate::{ProxyInputs, client};

type McpError = ErrorData;

pub mod metrics;
mod pool;
pub mod upstream;

const DELIMITER: &str = "_";

#[derive(Clone, Debug)]
pub struct RqCtx {
	identity: Identity,
	context: Context,
}

impl Default for RqCtx {
	fn default() -> Self {
		Self {
			identity: Identity::default(),
			context: Context::new(),
		}
	}
}

impl RqCtx {
	pub fn new(identity: Identity, context: Context) -> Self {
		Self { identity, context }
	}
}

#[derive(Clone)]
pub struct Relay {
	pool: Arc<RwLock<pool::ConnectionPool>>,
	metrics: Arc<metrics::Metrics>,
	policies: RuleSets,
	// If we have 1 target only, we don't prefix everything with 'target_'.
	// Else this is empty
	default_target_name: Option<String>,
}

impl Relay {
	pub fn new(
		pi: Arc<ProxyInputs>,
		backend: McpBackendGroup,
		metrics: Arc<metrics::Metrics>,
		policies: RuleSets,
		client: PolicyClient,
	) -> Self {
		let default_target_name = if backend.targets.len() != 1 {
			None
		} else {
			Some(backend.targets[0].name.to_string())
		};
		Self {
			pool: Arc::new(RwLock::new(pool::ConnectionPool::new(pi, client, backend))),
			metrics,
			policies,
			default_target_name,
		}
	}

	fn parse_resource_name<'a, 'b: 'a>(
		&'a self,
		res: &'b str,
	) -> Result<(&'a str, &'b str), McpError> {
		if let Some(default) = self.default_target_name.as_ref() {
			Ok((default.as_str(), res))
		} else {
			res
				.split_once(DELIMITER)
				.ok_or(McpError::invalid_request("invalid resource name", None))
		}
	}

	fn resource_name(&self, target: &str, name: &str) -> String {
		if self.default_target_name.is_none() {
			format!("{target}{DELIMITER}{name}")
		} else {
			name.to_string()
		}
	}

	fn setup_request(
		ext: &model::Extensions,
		span_name: &str,
	) -> Result<(BoxedSpan, RqCtx), McpError> {
		let (s, rq, _, _) = Self::setup_request_log(ext, span_name)?;
		Ok((s, rq))
	}
	fn setup_request_log(
		ext: &model::Extensions,
		span_name: &str,
	) -> Result<(BoxedSpan, RqCtx, AsyncLog<MCPInfo>, Arc<ContextBuilder>), McpError> {
		let Some(http) = ext.get::<Parts>() else {
			return Err(McpError::internal_error(
				"failed to extract parts".to_string(),
				None,
			));
		};
		let otelc = trcng::extract_context_from_request(&http.headers);
		let traceparent = http.extensions.get::<TraceParent>();
		let mut ctx = Context::new();
		if let Some(tp) = traceparent {
			ctx = ctx.with_remote_span_context(SpanContext::new(
				tp.trace_id.into(),
				tp.span_id.into(),
				TraceFlags::new(tp.flags),
				true,
				TraceState::default(),
			));
		}
		let claims = http.extensions.get::<Claims>();
		let tcp = http.extensions.get::<TCPConnectionInfo>();
		let tls = http.extensions.get::<TLSConnectionInfo>();
		let id = tls
			.and_then(|tls| tls.src_identity.as_ref())
			.map(|src_id| src_id.to_string());

		let log = http
			.extensions
			.get::<AsyncLog<MCPInfo>>()
			.cloned()
			.unwrap_or_default();

		let cel = http
			.extensions
			.get::<Arc<ContextBuilder>>()
			.cloned()
			.expect("CelContextBuilder must be set");

		let rq_ctx = RqCtx::new(Identity::new(claims.cloned(), id), ctx);

		let tracer = trcng::get_tracer();
		let _span = trcng::start_span(span_name.to_string(), &rq_ctx.identity)
			.with_kind(SpanKind::Server)
			.start_with_context(tracer, &rq_ctx.context);
		Ok((_span, rq_ctx, log, cel))
	}
}

impl Relay {
	pub async fn remove_target(&self, name: &str) -> Result<(), tokio::task::JoinError> {
		tracing::info!("removing target: {}", name);
		let mut pool = self.pool.write().await;
		match pool.remove(name).await {
			Some(target) => {
				match target.spec {
					upstream::UpstreamTargetSpec::Mcp(m) => {
						m.cancel().await?;
					},
					_ => {
						// Nothing to do here
					},
				}
				Ok(())
			},
			None => Ok(()),
		}
	}
}

// TODO: lists and gets can be macros
impl ServerHandler for Relay {
	#[instrument(level = "debug", skip_all)]
	fn get_info(&self) -> ServerInfo {
		ServerInfo {
            protocol_version: ProtocolVersion::V_2025_03_26,
            capabilities: ServerCapabilities {
                completions: None,
                experimental: None,
                logging: None,
                prompts: Some(PromptsCapability::default()),
                resources: Some(ResourcesCapability::default()),
                tools: Some(ToolsCapability::default()),
            },
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "This server is a gateway to a set of mcp servers. It is responsible for routing requests to the correct server and aggregating the results.".to_string(),
            ),
        }
	}

	// The client will send an initialize request with their parameters. We will return our own static support
	async fn initialize(
		&self,
		request: InitializeRequestParam,
		context: RequestContext<RoleServer>,
	) -> Result<InitializeResult, McpError> {
		let (_span, ref rq_ctx) = Self::setup_request(&context.extensions, "initialize")?;

		// List servers and initialize the ones that are not initialized
		let mut pool = self.pool.write().await;
		// Initialize all targets
		let connections = pool
			.initialize(rq_ctx, &context.peer, request)
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {e}"), None))?;

		// Return static server info about ourselves
		// TODO: we should actually perform an intersection of what the downstream and we support. The problem
		// is we may connect to many upstream servers, how do expose what exactly we can and cannot support?
		Ok(self.get_info())
	}

	#[instrument(level = "debug", skip_all)]
	async fn list_resources(
		&self,
		request: Option<PaginatedRequestParam>,
		context: RequestContext<RoleServer>,
	) -> std::result::Result<ListResourcesResult, McpError> {
		let (_span, ref rq_ctx) = Self::setup_request(&context.extensions, "list_resources")?;

		let mut pool = self.pool.write().await;
		let connections = pool
			.list()
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {e}"), None))?;
		let all = connections.into_iter().map(|(_name, svc)| {
			let request = request.clone();
			async move {
				match svc.list_resources(request, rq_ctx).await {
					Ok(r) => Ok(r.resources),
					Err(e) => Err(e),
				}
			}
		});

		// TODO: Handle errors
		let (results, _errors): (Vec<_>, Vec<_>) = futures::future::join_all(all)
			.await
			.into_iter()
			.partition_result();

		Ok(ListResourcesResult {
			resources: results.into_iter().flatten().collect(),
			next_cursor: None,
		})
	}

	#[instrument(level = "debug", skip_all)]
	async fn list_resource_templates(
		&self,
		request: Option<PaginatedRequestParam>,
		context: RequestContext<RoleServer>,
	) -> std::result::Result<ListResourceTemplatesResult, McpError> {
		let (_span, ref rq_ctx) = Self::setup_request(&context.extensions, "list_resource_templates")?;

		let mut pool = self.pool.write().await;
		let connections = pool
			.list()
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {e}"), None))?;
		let all = connections.into_iter().map(|(_name, svc)| {
			let request = request.clone();
			async move {
				match svc.list_resource_templates(request, rq_ctx).await {
					Ok(r) => Ok(r.resource_templates),
					Err(e) => Err(e),
				}
			}
		});

		let (results, _errors): (Vec<_>, Vec<_>) = futures::future::join_all(all)
			.await
			.into_iter()
			.partition_result();

		self.metrics.clone().record(
			metrics::ListCall {
				resource_type: "resource_template".to_string(),
				params: vec![],
			},
			(),
		);

		Ok(ListResourceTemplatesResult {
			resource_templates: results.into_iter().flatten().collect(),
			next_cursor: None,
		})
	}

	#[instrument(level = "debug", skip_all)]
	async fn list_prompts(
		&self,
		request: Option<PaginatedRequestParam>,
		context: RequestContext<RoleServer>,
	) -> std::result::Result<ListPromptsResult, McpError> {
		let (_span, ref rq_ctx) = Self::setup_request(&context.extensions, "list_prompts")?;

		let mut pool = self.pool.write().await;
		let connections = pool
			.list()
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {e}"), None))?;

		let all = connections.into_iter().map(|(_name, svc)| {
			let request = request.clone();
			async move {
				match svc.list_prompts(request, rq_ctx).await {
					Ok(r) => Ok(
						r.prompts
							.into_iter()
							.map(|p| Prompt {
								name: self.resource_name(_name.as_str(), &p.name),
								description: p.description,
								arguments: p.arguments,
							})
							.collect::<Vec<_>>(),
					),
					Err(e) => Err(e),
				}
			}
		});

		let (results, _errors): (Vec<_>, Vec<_>) = futures::future::join_all(all)
			.await
			.into_iter()
			.partition_result();

		self.metrics.record(
			metrics::ListCall {
				resource_type: "prompt".to_string(),
				params: vec![],
			},
			(),
		);
		Ok(ListPromptsResult {
			prompts: results.into_iter().flatten().collect(),
			next_cursor: None,
		})
	}

	#[instrument(
        level = "debug",
        skip_all,
        fields(
        name=%request.uri,
        ),
    )]
	async fn read_resource(
		&self,
		request: ReadResourceRequestParam,
		context: RequestContext<RoleServer>,
	) -> std::result::Result<ReadResourceResult, McpError> {
		let (_span, ref rq_ctx, _, cel) =
			Self::setup_request_log(&context.extensions, "read_resource")?;

		let uri = request.uri.to_string();
		let (service_name, resource) = self.parse_resource_name(&uri)?;
		if !self.policies.validate(
			&rbac::ResourceType::Resource(rbac::ResourceId::new(
				service_name.to_string(),
				resource.to_string(),
			)),
			cel.as_ref(),
		) {
			return Err(McpError::invalid_request("not allowed", None));
		}

		let mut pool = self.pool.write().await;
		let service_arc = pool
			.get(rq_ctx, &context.peer, service_name)
			.await
			.map_err(|_e| McpError::invalid_request(format!("Service {service_name} not found"), None))?;
		let req = ReadResourceRequestParam {
			uri: resource.to_string(),
		};

		self.metrics.clone().record(
			metrics::GetResourceCall {
				server: service_name.to_string(),
				uri: resource.to_string(),
				params: vec![],
			},
			(),
		);
		match service_arc.read_resource(req, rq_ctx).await {
			Ok(r) => Ok(r),
			Err(e) => Err(e.into()),
		}
	}

	#[instrument(
        level = "debug",
        skip_all,
        fields(
        name=%request.name,
        ),
    )]
	async fn get_prompt(
		&self,
		request: GetPromptRequestParam,
		context: RequestContext<RoleServer>,
	) -> std::result::Result<GetPromptResult, McpError> {
		let (_span, ref rq_ctx, _, cel) = Self::setup_request_log(&context.extensions, "get_prompt")?;

		let prompt_name = request.name.to_string();
		let (service_name, prompt) = self.parse_resource_name(&prompt_name)?;
		if !self.policies.validate(
			&rbac::ResourceType::Prompt(rbac::ResourceId::new(
				service_name.to_string(),
				prompt.to_string(),
			)),
			cel.as_ref(),
		) {
			return Err(McpError::invalid_request("not allowed", None));
		}
		let mut pool = self.pool.write().await;
		let svc = pool
			.get(rq_ctx, &context.peer, service_name)
			.await
			.map_err(|_e| McpError::invalid_request(format!("Service {service_name} not found"), None))?;
		let req = GetPromptRequestParam {
			name: prompt.to_string(),
			arguments: request.arguments,
		};

		self.metrics.clone().record(
			metrics::GetPromptCall {
				server: service_name.to_string(),
				name: prompt.to_string(),
				params: vec![],
			},
			(),
		);
		match svc.get_prompt(req, rq_ctx).await {
			Ok(r) => Ok(r),
			Err(e) => Err(e.into()),
		}
	}

	#[instrument(level = "debug", skip_all)]
	async fn list_tools(
		&self,
		request: Option<PaginatedRequestParam>,
		mut context: RequestContext<RoleServer>,
	) -> std::result::Result<ListToolsResult, McpError> {
		let (_span, ref rq_ctx, _, cel) = Self::setup_request_log(&context.extensions, "list_tools")?;
		let mut pool = self.pool.write().await;
		let connections = pool
			.list()
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {e}"), None))?;
		let multi = connections.len() > 1;
		let all = connections.into_iter().map(|(_name, svc_arc)| {
			let request = request.clone();
			let cel = cel.clone();
			async move {
				match svc_arc.list_tools(request, rq_ctx).await {
					Ok(r) => Ok(
						r.tools
							.into_iter()
							.filter(|t| {
								self.policies.validate(
									&rbac::ResourceType::Tool(rbac::ResourceId::new(
										_name.to_string(),
										t.name.to_string(),
									)),
									cel.as_ref(),
								)
							})
							.map(|t| Tool {
								annotations: None,
								name: Cow::Owned(self.resource_name(_name.as_str(), &t.name)),
								description: t.description,
								input_schema: t.input_schema,
								output_schema: t.output_schema,
							})
							.collect::<Vec<_>>(),
					),
					Err(e) => Err(e),
				}
			}
		});

		let (results, _errors): (Vec<_>, Vec<_>) = futures::future::join_all(all)
			.await
			.into_iter()
			.partition_result();

		self.metrics.clone().record(
			metrics::ListCall {
				resource_type: "tool".to_string(),
				params: vec![],
			},
			(),
		);

		Ok(ListToolsResult {
			tools: results.into_iter().flatten().collect(),
			next_cursor: None,
		})
	}

	#[instrument(
        level = "debug",
        skip_all,
        fields(
        name=%request.name,
        ),
    )]
	fn call_tool(
		&self,
		request: CallToolRequestParam,
		context: RequestContext<RoleServer>,
	) -> impl Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
		Box::pin(async move {
			let (_span, ref rq_ctx, log, cel) =
				Self::setup_request_log(&context.extensions, "call_tool")?;
			let tool_name = request.name.to_string();
			let (service_name, tool) = self.parse_resource_name(&tool_name)?;
			log.non_atomic_mutate(|l| {
				l.tool_call_name = Some(tool.to_string());
				l.target_name = Some(service_name.to_string());
			});
			if !self.policies.validate(
				&rbac::ResourceType::Tool(rbac::ResourceId::new(
					service_name.to_string(),
					tool.to_string(),
				)),
				cel.as_ref(),
			) {
				return Err(McpError::invalid_request("not allowed", None));
			}
			let mut pool = self.pool.write().await;
			let svc = pool
				.get(rq_ctx, &context.peer, service_name)
				.await
				.map_err(|_e| {
					McpError::invalid_request(format!("Service {service_name} not found"), None)
				})?;
			let req = CallToolRequestParam {
				name: Cow::Owned(tool.to_string()),
				arguments: request.arguments,
			};

			self.metrics.record(
				metrics::ToolCall {
					server: service_name.to_string(),
					name: tool.to_string(),
					params: vec![],
				},
				(),
			);

			match svc.call_tool(req, rq_ctx).await {
				Ok(r) => Ok(r),
				Err(e) => {
					self.metrics.record(
						metrics::ToolCallError {
							server: service_name.to_string(),
							name: tool.to_string(),
							error_type: e.error_code(),
							params: vec![],
						},
						(),
					);
					Err(e.into())
				},
			}
		})
	}
}
