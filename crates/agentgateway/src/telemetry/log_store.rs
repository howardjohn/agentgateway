mod postgres;
mod sqlite;

use std::collections::BTreeMap;
use std::sync::Arc;
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

#[derive(Clone)]
pub struct RequestLogStore {
	tx: mpsc::Sender<StoredRequestLog>,
	backend: Arc<Backend>,
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
	let backend = Arc::new(Backend::connect(cfg).await?);
	let (tx, mut rx) = mpsc::channel(BUFFERED_RECORDS);
	let store = RequestLogStore {
		tx,
		backend: backend.clone(),
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

pub async fn search(request: SearchRequest) -> anyhow::Result<SearchResponse> {
	let store = REQUEST_LOG_STORE
		.get()
		.ok_or_else(|| anyhow::anyhow!("request log database is not configured"))?;
	store.backend.search(request).await
}

pub async fn get(request: GetRequest) -> anyhow::Result<GetResponse> {
	let store = REQUEST_LOG_STORE
		.get()
		.ok_or_else(|| anyhow::anyhow!("request log database is not configured"))?;
	store.backend.get(request).await
}

pub async fn token_usage(request: TokenUsageRequest) -> anyhow::Result<TokenUsageResponse> {
	let store = REQUEST_LOG_STORE
		.get()
		.ok_or_else(|| anyhow::anyhow!("request log database is not configured"))?;
	store.backend.token_usage(request).await
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimeRange {
	pub from: Option<DateTime<Utc>>,
	pub to: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogFilters {
	#[serde(default)]
	pub http_status: Vec<i64>,
	#[serde(default)]
	pub provider: Vec<String>,
	#[serde(default)]
	pub request_model: Vec<String>,
	#[serde(default)]
	pub response_model: Vec<String>,
	#[serde(default)]
	pub trace_id: Option<String>,
	#[serde(default)]
	pub has_payload: Option<bool>,
	#[serde(default)]
	pub attributes: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchRequest {
	#[serde(default)]
	pub limit: Option<i64>,
	#[serde(default)]
	pub cursor: Option<String>,
	#[serde(default)]
	pub time_range: Option<TimeRange>,
	#[serde(default)]
	pub filters: LogFilters,
	#[serde(default)]
	pub include_attributes: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetRequest {
	pub id: String,
	#[serde(default)]
	pub include_payload: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenUsageRequest {
	#[serde(default)]
	pub time_range: Option<TimeRange>,
	#[serde(default)]
	pub filters: LogFilters,
	#[serde(default)]
	pub group_by: Vec<GroupBy>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupBy {
	pub field: GroupByField,
	#[serde(default)]
	pub key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum GroupByField {
	Provider,
	RequestModel,
	ResponseModel,
	HttpStatus,
	Attributes,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
	pub logs: Vec<LogEntry>,
	pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResponse {
	pub log: Option<LogEntry>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageResponse {
	pub groups: Vec<TokenUsageGroup>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageGroup {
	pub group: BTreeMap<String, Value>,
	pub requests: i64,
	pub input_tokens: i64,
	pub output_tokens: i64,
	pub total_tokens: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
	pub id: String,
	pub started_at: DateTime<Utc>,
	pub completed_at: DateTime<Utc>,
	pub duration_ms: i64,
	pub trace_id: Option<String>,
	pub span_id: Option<String>,
	pub http_status: Option<i64>,
	pub error: Option<String>,
	pub gen_ai: GenAiEntry,
	pub usage: UsageEntry,
	pub has_payload: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub attributes: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub payload: Option<PayloadEntry>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenAiEntry {
	pub operation_name: Option<String>,
	pub provider_name: Option<String>,
	pub request_model: Option<String>,
	pub response_model: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEntry {
	pub input_tokens: Option<i64>,
	pub output_tokens: Option<i64>,
	pub total_tokens: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadEntry {
	pub request_prompt: Option<Value>,
	pub response_completion: Option<Value>,
}

pub(crate) fn limit(limit: Option<i64>) -> i64 {
	limit.unwrap_or(100).clamp(1, 500)
}

pub(crate) fn encode_cursor(completed_at: DateTime<Utc>, id: &str) -> String {
	format!("{}|{}", completed_at.to_rfc3339(), id)
}

pub(crate) fn decode_cursor(cursor: &str) -> anyhow::Result<(DateTime<Utc>, String)> {
	let (completed_at, id) = cursor
		.split_once('|')
		.ok_or_else(|| anyhow::anyhow!("invalid cursor"))?;
	Ok((completed_at.parse::<DateTime<Utc>>()?, id.to_string()))
}

pub(crate) fn attr_value(value: &Value) -> Option<String> {
	match value {
		Value::Null => None,
		Value::Bool(value) => Some(value.to_string()),
		Value::Number(value) => Some(value.to_string()),
		Value::String(value) => Some(value.clone()),
		Value::Array(_) | Value::Object(_) => None,
	}
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

	async fn search(&self, request: SearchRequest) -> anyhow::Result<SearchResponse> {
		match self {
			Self::Sqlite(store) => store.search(request).await,
			Self::Postgres(store) => store.search(request).await,
		}
	}

	async fn get(&self, request: GetRequest) -> anyhow::Result<GetResponse> {
		match self {
			Self::Sqlite(store) => store.get(request).await,
			Self::Postgres(store) => store.get(request).await,
		}
	}

	async fn token_usage(&self, request: TokenUsageRequest) -> anyhow::Result<TokenUsageResponse> {
		match self {
			Self::Sqlite(store) => store.token_usage(request).await,
			Self::Postgres(store) => store.token_usage(request).await,
		}
	}
}
