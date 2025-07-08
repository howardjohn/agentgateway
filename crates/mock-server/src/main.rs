use std::env;

use mock_server::Server;

#[tokio::main]
async fn main() {
	let port = env::var("PORT")
		.ok()
		.and_then(|s| s.parse().ok())
		.unwrap_or(8080);
	let server = Server::run_with_port(port).await;
	println!("Listening on {}", server.address());
	server.wait_for_shutdown().await;
}
