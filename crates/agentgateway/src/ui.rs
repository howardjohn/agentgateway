use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use http::{HeaderName, HeaderValue, Method};
use hyper::body::Incoming;
use include_dir::{Dir, include_dir};
use serde_json::Value;
use tower::ServiceExt;
use tower_http::cors::CorsLayer;
use tower_serve_static::ServeDir;

use crate::management::admin::{AdminFallback, AdminResponse, ConfigDumpHandler};
pub struct UiHandler {
	router: Router,
}

#[derive(Clone, Debug)]
struct App {
	state: (),
}

lazy_static::lazy_static! {
	static ref ASSETS_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../ui/out");
}

impl UiHandler {
	pub fn new() -> Self {
		let ui_service = ServeDir::new(&ASSETS_DIR);
		let router = Router::new()
			// Redirect to the UI
			.route("/targets/mcp", get(targets_mcp_list_handler))
			.nest_service("/ui", ui_service)
			.route("/", get(|| async { Redirect::permanent("/ui") }))
			.layer(add_cors_layer())
			.with_state(App { state: () });
		Self { router }
	}
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ErrorResponse {
	message: String,
}

impl IntoResponse for ErrorResponse {
	fn into_response(self) -> Response {
		(StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
	}
}

async fn targets_mcp_list_handler(
	State(app): State<App>,
) -> Result<String, (StatusCode, impl IntoResponse)> {
	Err((
		StatusCode::INTERNAL_SERVER_ERROR,
		ErrorResponse {
			message: "not implemented".to_string(),
		},
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

impl AdminFallback for UiHandler {
	fn handle(&self, req: http::Request<Incoming>) -> AdminResponse {
		let router = self.router.clone();
		Box::pin(async { router.oneshot(req).await.unwrap() })
	}
}
