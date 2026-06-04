mod postgres;
mod sqlite;

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

static REQUEST_LOG_STORE: OnceLock<RequestLogStore> = OnceLock::new();
const BUFFERED_RECORDS: usize = 10_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
	pub url: String,
	#[serde(default)]
	pub store_payloads: bool,
}

#[derive(Clone, Debug)]
pub struct RequestLogStore {
	tx: mpsc::Sender<StoredRequestLog>,
	store_payloads: bool,
}

impl RequestLogStore {
	pub fn emit(&self, mut record: StoredRequestLog) {
		if !self.store_payloads {
			record.payload = None;
			record.has_payload = false;
		}
		if let Err(err) = self.tx.try_send(record) {
			warn!(target: "request", ?err, "dropping request log database record");
		}
	}
}

pub async fn setup(cfg: &Config) -> anyhow::Result<RequestLogStoreGuard> {
	let backend = Backend::connect(cfg).await?;
	let (tx, mut rx) = mpsc::channel(BUFFERED_RECORDS);
	let store = RequestLogStore {
		tx,
		store_payloads: cfg.store_payloads,
	};
	let _ = REQUEST_LOG_STORE.set(store);
	let writer = tokio::spawn(async move {
		while let Some(record) = rx.recv().await {
			if let Err(err) = backend.insert(record).await {
				warn!(target: "request", ?err, "failed to persist request log");
			}
		}
		debug!(target: "request", "request log database writer stopped");
	});
	Ok(RequestLogStoreGuard { writer })
}

pub fn emit(record: StoredRequestLog) {
	if let Some(store) = REQUEST_LOG_STORE.get() {
		store.emit(record);
	}
}

pub fn enabled() -> bool {
	REQUEST_LOG_STORE.get().is_some()
}

pub struct RequestLogStoreGuard {
	writer: tokio::task::JoinHandle<()>,
}

impl Drop for RequestLogStoreGuard {
	fn drop(&mut self) {
		self.writer.abort();
	}
}

#[derive(Clone, Debug)]
pub struct StoredRequestLog {
	pub id: String,
	pub started_at: DateTime<Utc>,
	pub completed_at: DateTime<Utc>,
	pub duration_ms: i64,
	pub trace_id: Option<String>,
	pub span_id: Option<String>,
	pub http_status: Option<i64>,
	pub error: Option<String>,
	pub gen_ai_operation_name: Option<String>,
	pub gen_ai_provider_name: Option<String>,
	pub gen_ai_request_model: Option<String>,
	pub gen_ai_response_model: Option<String>,
	pub input_tokens: Option<i64>,
	pub output_tokens: Option<i64>,
	pub total_tokens: Option<i64>,
	pub has_payload: bool,
	pub attributes_json: Value,
	pub payload: Option<StoredRequestLogPayload>,
}

#[derive(Clone, Debug)]
pub struct StoredRequestLogPayload {
	pub request_prompt_json: Option<Value>,
	pub response_completion_json: Option<Value>,
}

enum Backend {
	Sqlite(sqlite::SqliteLogStore),
	Postgres(postgres::PostgresLogStore),
}

impl Backend {
	async fn connect(cfg: &Config) -> anyhow::Result<Self> {
		if cfg.url.starts_with("postgres://") || cfg.url.starts_with("postgresql://") {
			Ok(Self::Postgres(
				postgres::PostgresLogStore::connect(&cfg.url).await?,
			))
		} else {
			Ok(Self::Sqlite(
				sqlite::SqliteLogStore::connect(&cfg.url).await?,
			))
		}
	}

	async fn insert(&self, record: StoredRequestLog) -> anyhow::Result<()> {
		match self {
			Self::Sqlite(store) => store.insert(record).await,
			Self::Postgres(store) => store.insert(record).await,
		}
	}
}
