use std::collections::HashMap;
use std::net::SocketAddr;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Uri};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct EchoResponse {
	pub method: String,
	pub path: String,
	pub headers: HashMap<String, String>,
	pub body: String,
}

pub struct Server {
	address: SocketAddr,
	shutdown: tokio::sync::oneshot::Sender<()>,
	handle: tokio::task::JoinHandle<()>,
}

impl Server {
	pub async fn run() -> Self {
		Self::run_with_port(0).await
	}

	pub async fn run_with_port(port: u16) -> Self {
		let listener = TcpListener::bind(("127.0.0.1", port))
			.await
			.expect("Failed to bind");
		let address = listener.local_addr().expect("Failed to get local addr");
		let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

		let app = Router::new().route("/echo", axum::routing::any(echo_handler));
		let server = async move {
			axum::serve(listener, app)
				.with_graceful_shutdown(async move {
					let _ = shutdown_rx.await;
				})
				.await
				.expect("server error");
		};

		let handle = tokio::spawn(server);

		Server {
			address,
			shutdown: shutdown_tx,
			handle,
		}
	}

	pub fn address(&self) -> SocketAddr {
		self.address
	}

	pub async fn shutdown(self) {
		let _ = self.shutdown.send(());
		let _ = self.handle.await;
	}

	pub async fn wait_for_shutdown(self) {
		let _ = self.handle.await;
	}
}

async fn echo_handler(
	method: Method,
	uri: Uri,
	headers: HeaderMap,
	body: Bytes,
) -> Json<EchoResponse> {
	let headers_map: HashMap<String, String> = headers
		.iter()
		.map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
		.collect();
	let body_str = String::from_utf8(body.to_vec()).unwrap_or_else(|_| "<non-utf8 body>".to_string());
	let resp = EchoResponse {
		method: method.to_string(),
		path: uri.path().to_string(),
		headers: headers_map,
		body: body_str,
	};
	Json(resp)
}
