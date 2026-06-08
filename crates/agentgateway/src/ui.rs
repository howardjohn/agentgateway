use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Sse;
use axum::response::sse::Event;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use http::{HeaderName, HeaderValue, Method};
use hyper::body::Incoming;
use include_dir::{Dir, include_dir};
use serde::{Serialize, Serializer};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower::ServiceExt;
use tower_http::cors::CorsLayer;
use tower_serve_static::ServeDir;

use crate::cel::{self, ExecutorSerde};
use crate::management::admin::{AdminFallback, AdminResponse};
use crate::{Config, ConfigSource, client, yamlviajson};
pub struct UiHandler {
	router: Router,
}

#[derive(Clone, Debug)]
struct App {
	state: Arc<Config>,
	client: client::Client,
}

impl App {
	pub fn cfg(&self) -> Result<ConfigSource, ErrorResponse> {
		self
			.state
			.xds
			.local_config
			.clone()
			.ok_or(ErrorResponse::String("local config not setup".to_string()))
	}
}

lazy_static::lazy_static! {
	static ref ASSETS_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../ui/out");
}

impl UiHandler {
	pub fn new(cfg: Arc<Config>) -> Self {
		let ui_service = ServeDir::new(&ASSETS_DIR);
		let router = Router::new()
			// Redirect to the UI
			.route("/config", get(get_config).post(write_config))
			.route("/cel", axum::routing::post(handle_cel))
			.route("/api/logs/search", post(search_logs))
			.route("/api/logs/get", post(get_log))
			.route("/api/logs/tail", post(tail_logs))
			.route("/api/logs/analytics/token-usage", post(token_usage))
			.nest_service("/ui", ui_service)
			.route("/", get(|| async { Redirect::permanent("/ui") }))
			.layer(add_cors_layer())
			.with_state(App {
				state: cfg.clone(),
				client: client::Client::new(&cfg.dns, None, Default::default(), None),
			});
		Self { router }
	}
}

#[derive(Debug, thiserror::Error)]
enum ErrorResponse {
	#[error("{0}")]
	String(String),
	#[error("{0}")]
	Anyhow(#[from] anyhow::Error),
}

impl Serialize for ErrorResponse {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		self.to_string().serialize(serializer)
	}
}

impl IntoResponse for ErrorResponse {
	fn into_response(self) -> Response {
		(StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
	}
}

async fn get_config(State(app): State<App>) -> Result<Json<Value>, ErrorResponse> {
	let s = app.cfg()?.read_to_string().await?;
	let v: Value = yamlviajson::from_str(&s).map_err(|e| ErrorResponse::Anyhow(e.into()))?;
	Ok(Json(v))
}

async fn write_config(
	State(app): State<App>,
	Json(config_json): Json<Value>,
) -> Result<Json<Value>, ErrorResponse> {
	let config_source = app.cfg()?;

	let file_path = match &config_source {
		ConfigSource::File(path) => path,
		ConfigSource::Static(_) => {
			return Err(ErrorResponse::String(
				"Cannot write to static config".to_string(),
			));
		},
	};
	let yaml_content =
		yamlviajson::to_string(&config_json).map_err(|e| ErrorResponse::Anyhow(e.into()))?;

	if let Err(e) = crate::types::local::NormalizedLocalConfig::from(
		&app.state,
		app.client.clone(),
		app.state.gateway(),
		yaml_content.as_str(),
	)
	.await
	{
		return Err(ErrorResponse::String(e.to_string()));
	}

	// Write the YAML content to the file
	fs_err::tokio::write(file_path, yaml_content)
		.await
		.map_err(|e| ErrorResponse::Anyhow(e.into()))?;

	// Return success response
	Ok(Json(
		serde_json::json!({"status": "success", "message": "Configuration written successfully"}),
	))
}

pub fn add_cors_layer() -> CorsLayer {
	CorsLayer::new()
		.allow_origin(
			[
				"http://0.0.0.0:3000",
				"http://localhost:3000",
				"http://127.0.0.1:3000",
				"http://0.0.0.0:19000",
				"http://127.0.0.1:19000",
				"http://localhost:19000",
			]
			.map(|origin| origin.parse::<HeaderValue>().unwrap()),
		)
		.allow_headers([
			CONTENT_TYPE,
			AUTHORIZATION,
			HeaderName::from_static("x-requested-with"),
		])
		.allow_methods([
			Method::GET,
			Method::POST,
			Method::PUT,
			Method::DELETE,
			Method::OPTIONS,
		])
		.allow_credentials(true)
		.expose_headers([CONTENT_TYPE, CONTENT_LENGTH])
		.max_age(Duration::from_secs(3600))
}

#[derive(serde::Deserialize)]
struct CelRequest {
	expression: String,
	#[serde(default)]
	data: Option<serde_json::Value>,
}

#[derive(serde::Serialize)]
struct CelResponse {
	result: Option<serde_json::Value>,
	error: Option<String>,
}

async fn handle_cel(Json(request): Json<CelRequest>) -> Response {
	// Compile the expression
	let expression = match cel::Expression::new_strict(&request.expression) {
		Ok(expr) => expr,
		Err(e) => {
			let resp = CelResponse {
				result: None,
				error: Some(format!("Failed to compile expression: {}", e)),
			};
			return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
		},
	};

	// Deserialize the input data or use empty data if not provided
	let executor_serde: ExecutorSerde = match request.data {
		Some(data) => match serde_json::from_value(data) {
			Ok(serde) => serde,
			Err(e) => {
				let resp = CelResponse {
					result: None,
					error: Some(format!("Failed to parse input data: {}", e)),
				};
				return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
			},
		},
		_ => ExecutorSerde::default(),
	};

	// Create the executor and evaluate the expression
	let executor = executor_serde.as_executor();
	let resp = match executor.eval(&expression) {
		Ok(value) => match value.json() {
			Ok(json) => CelResponse {
				result: Some(json),
				error: None,
			},
			Err(e) => CelResponse {
				result: None,
				error: Some(format!("Failed to convert result to JSON: {}", e)),
			},
		},
		Err(e) => CelResponse {
			result: None,
			error: Some(format!("Evaluation error: {}", e)),
		},
	};

	(StatusCode::OK, Json(resp)).into_response()
}

async fn search_logs(
	Json(request): Json<crate::telemetry::log_store::SearchRequest>,
) -> Result<Json<crate::telemetry::log_store::SearchResponse>, ErrorResponse> {
	crate::telemetry::log_store::search(request)
		.await
		.map(Json)
		.map_err(ErrorResponse::Anyhow)
}

async fn get_log(
	Json(request): Json<crate::telemetry::log_store::GetRequest>,
) -> Result<Json<crate::telemetry::log_store::GetResponse>, ErrorResponse> {
	crate::telemetry::log_store::get(request)
		.await
		.map(Json)
		.map_err(ErrorResponse::Anyhow)
}

async fn token_usage(
	Json(request): Json<crate::telemetry::log_store::TokenUsageRequest>,
) -> Result<Json<crate::telemetry::log_store::TokenUsageResponse>, ErrorResponse> {
	crate::telemetry::log_store::token_usage(request)
		.await
		.map(Json)
		.map_err(ErrorResponse::Anyhow)
}

async fn tail_logs(
	Json(mut request): Json<crate::telemetry::log_store::TailRequest>,
) -> Result<Sse<ReceiverStream<Result<Event, std::convert::Infallible>>>, ErrorResponse> {
	if !crate::telemetry::log_store::enabled() {
		return Err(ErrorResponse::String(
			"request log database is not configured".to_string(),
		));
	}
	let mut cursor = request
		.cursor
		.clone()
		.or_else(|| Some(crate::telemetry::log_store::encode_cursor(Utc::now(), "")));
	request.limit = Some(request.limit.unwrap_or(100).clamp(1, 500));

	let (tx, rx) = mpsc::channel(32);
	tokio::spawn(async move {
		let mut poll = tokio::time::interval(Duration::from_secs(1));
		let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
		loop {
			tokio::select! {
				_ = poll.tick() => {
					let mut batch_request = request.clone();
					batch_request.cursor = cursor.clone();
					match crate::telemetry::log_store::tail(batch_request).await {
						Ok(response) => {
							for log in response.logs {
								let next = crate::telemetry::log_store::encode_cursor(log.completed_at, &log.id);
								cursor = Some(next.clone());
								let event = crate::telemetry::log_store::TailEvent {
									entry: log,
									cursor: next,
								};
								let Ok(data) = serde_json::to_string(&event) else {
									continue;
								};
								if tx.send(Ok(Event::default().event("log").data(data))).await.is_err() {
									return;
								}
							}
							if let Some(next) = response.next_cursor {
								cursor = Some(next);
							}
						},
						Err(err) => {
							let event = Event::default()
								.event("error")
								.data(serde_json::json!({ "message": err.to_string() }).to_string());
							let _ = tx.send(Ok(event)).await;
							return;
						},
					}
				},
				_ = heartbeat.tick() => {
					if tx.send(Ok(Event::default().event("heartbeat").data("{}"))).await.is_err() {
						return;
					}
				},
			}
		}
	});

	Ok(Sse::new(ReceiverStream::new(rx)))
}

impl AdminFallback for UiHandler {
	fn handle(&self, req: http::Request<Incoming>) -> AdminResponse {
		let router = self.router.clone();
		Box::pin(async { router.oneshot(req).await.unwrap() })
	}
}
