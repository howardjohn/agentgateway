use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use agent_hbone::{Config, H2Config, Key, TokioH2Stream};
use bytes::Bytes;
use http::{Method, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

const END_METADATA: u8 = 0x4;
const METADATA_A: &[u8] = b"\x00\x05state\x01A";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TestKey(SocketAddr);

impl fmt::Display for TestKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(f)
	}
}

impl Key for TestKey {
	fn dest(&self) -> SocketAddr {
		self.0
	}
}

#[tokio::test]
async fn basic_hbone_tunnel() {
	let (client_io, server_io) = tokio::io::duplex(64 * 1024);
	let (_drain_trigger, drain) = agent_core::drain::new();
	let (_force_tx, force_rx) = watch::channel(());

	let server = tokio::spawn(agent_hbone::server::serve_connection(
		Arc::new(Config::default()),
		server_io,
		(),
		drain,
		force_rx,
		|request, (), _drain| async move {
			let response = Response::builder().status(200).body(()).unwrap();
			let stream = request.send_response(response).await.unwrap();
			let mut hbone = TokioH2Stream::new(stream);

			let (mut target, echo) = tokio::io::duplex(1024);
			let echo = tokio::spawn(async move {
				let (mut read, mut write) = tokio::io::split(echo);
				tokio::io::copy(&mut read, &mut write).await.unwrap();
			});

			tokio::io::copy_bidirectional(&mut hbone, &mut target)
				.await
				.unwrap();
			echo.await.unwrap();
		},
	));

	let (_driver_tx, driver_rx) = watch::channel(false);
	let key = TestKey("127.0.0.1:80".parse().unwrap());
	let mut client =
		agent_hbone::client::spawn_connection(&H2Config::default(), client_io, driver_rx, key)
			.await
			.unwrap();
	let request = Request::builder()
		.method(Method::CONNECT)
		.uri("example.test:80")
		.body(())
		.unwrap();
	let mut tunnel = TokioH2Stream::new(client.send_request(request).await.unwrap());

	tunnel.write_all(b"hello").await.unwrap();
	let mut echoed = [0; 5];
	tunnel.read_exact(&mut echoed).await.unwrap();
	assert_eq!(&echoed, b"hello");

	server.abort();
}

#[tokio::test]
async fn metadata_is_observed_before_later_data() {
	let _ = tracing_subscriber::fmt()
		.with_env_filter("agent_hbone=info")
		.with_test_writer()
		.try_init();

	let (client_io, server_io) = tokio::io::duplex(64 * 1024);
	let (_drain_trigger, drain) = agent_core::drain::new();
	let (_force_tx, force_rx) = watch::channel(());
	let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();

	let server = tokio::spawn(agent_hbone::server::serve_connection(
		Arc::new(Config::default()),
		server_io,
		observed_tx,
		drain,
		force_rx,
		|request, observed, _drain| async move {
			let response = Response::builder().status(200).body(()).unwrap();
			let stream = request.send_response(response).await.unwrap();
			let metadata = stream.metadata();
			let mut hbone = TokioH2Stream::new(stream);
			let (mut target, mut tunneled) = tokio::io::duplex(1024);
			let copy = tokio::spawn(async move {
				tokio::io::copy_bidirectional(&mut hbone, &mut target)
					.await
					.unwrap();
			});

			for _ in 0..2 {
				let mut data = [0; 3];
				tunneled.read_exact(&mut data).await.unwrap();
				let metadata = metadata.load();
				observed
					.send((metadata, Bytes::copy_from_slice(&data)))
					.unwrap();
			}
			copy.abort();
		},
	));

	let (_driver_tx, driver_rx) = watch::channel(false);
	let key = TestKey("127.0.0.1:80".parse().unwrap());
	let mut client =
		agent_hbone::client::spawn_connection(&H2Config::default(), client_io, driver_rx, key)
			.await
			.unwrap();
	let request = Request::builder()
		.method(Method::CONNECT)
		.uri("example.test:80")
		.body(())
		.unwrap();
	let stream = client.send_request(request).await.unwrap();
	let extensions = stream.extension_sender();
	let mut tunnel = TokioH2Stream::new(stream);

	tunnel.write_all(b"one").await.unwrap();
	tokio::time::sleep(Duration::from_millis(100)).await;
	extensions
		.send_extension(
			agent_hbone::METADATA_FRAME_TYPE,
			END_METADATA,
			METADATA_A.into(),
		)
		.unwrap();
	tunnel.write_all(b"two").await.unwrap();

	let (_, first_data) = observed_rx.recv().await.unwrap();
	assert_eq!(first_data, "one");
	let (metadata, data) = observed_rx.recv().await.unwrap();
	assert_eq!(data, "two");
	assert_eq!(metadata.as_deref(), Some(METADATA_A));

	server.abort();
}
