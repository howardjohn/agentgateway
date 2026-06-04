use std::time::Duration;

use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::types::Json;

use super::StoredRequestLog;

pub struct SqliteLogStore {
	pool: SqlitePool,
}

impl SqliteLogStore {
	pub async fn connect(url: &str) -> anyhow::Result<Self> {
		let options = url
			.parse::<SqliteConnectOptions>()
			.context("failed to parse request log sqlite database URL")?
			.create_if_missing(true)
			.journal_mode(SqliteJournalMode::Wal)
			.synchronous(SqliteSynchronous::Normal)
			.busy_timeout(Duration::from_secs(5));
		let pool = SqlitePoolOptions::new()
			.max_connections(5)
			.connect_with(options)
			.await
			.context("failed to connect request log sqlite database")?;
		sqlx::raw_sql(SCHEMA).execute(&pool).await?;
		Ok(Self { pool })
	}

	pub async fn insert(&self, record: StoredRequestLog) -> anyhow::Result<()> {
		let mut tx = self.pool.begin().await?;
		sqlx::query(INSERT_LOG)
			.bind(&record.id)
			.bind(record.started_at)
			.bind(record.completed_at)
			.bind(record.duration_ms)
			.bind(&record.trace_id)
			.bind(&record.span_id)
			.bind(record.http_status)
			.bind(&record.error)
			.bind(&record.gen_ai_operation_name)
			.bind(&record.gen_ai_provider_name)
			.bind(&record.gen_ai_request_model)
			.bind(&record.gen_ai_response_model)
			.bind(record.input_tokens)
			.bind(record.output_tokens)
			.bind(record.total_tokens)
			.bind(record.has_payload)
			.bind(Json(record.attributes_json))
			.execute(&mut *tx)
			.await?;
		if let Some(payload) = record.payload {
			sqlx::query(INSERT_PAYLOAD)
				.bind(&record.id)
				.bind(payload.request_prompt_json.map(Json))
				.bind(payload.response_completion_json.map(Json))
				.execute(&mut *tx)
				.await?;
		}
		tx.commit().await?;
		Ok(())
	}
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS request_logs (
	id TEXT PRIMARY KEY,
	started_at TEXT NOT NULL,
	completed_at TEXT NOT NULL,
	duration_ms INTEGER NOT NULL,
	trace_id TEXT,
	span_id TEXT,
	http_status INTEGER,
	error TEXT,
	gen_ai_operation_name TEXT,
	gen_ai_provider_name TEXT,
	gen_ai_request_model TEXT,
	gen_ai_response_model TEXT,
	input_tokens INTEGER,
	output_tokens INTEGER,
	total_tokens INTEGER,
	has_payload INTEGER NOT NULL,
	attributes_json TEXT NOT NULL CHECK (json_valid(attributes_json))
);

CREATE TABLE IF NOT EXISTS request_log_payloads (
	log_id TEXT PRIMARY KEY REFERENCES request_logs(id) ON DELETE CASCADE,
	request_prompt_json TEXT CHECK (request_prompt_json IS NULL OR json_valid(request_prompt_json)),
	response_completion_json TEXT CHECK (response_completion_json IS NULL OR json_valid(response_completion_json))
);

CREATE INDEX IF NOT EXISTS idx_request_logs_completed_at ON request_logs(completed_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_request_logs_http_status_completed_at ON request_logs(http_status, completed_at DESC);
CREATE INDEX IF NOT EXISTS idx_request_logs_gen_ai_completed_at ON request_logs(gen_ai_provider_name, gen_ai_request_model, completed_at DESC);
"#;

const INSERT_LOG: &str = r#"
INSERT INTO request_logs (
	id, started_at, completed_at, duration_ms, trace_id, span_id, http_status, error,
	gen_ai_operation_name, gen_ai_provider_name, gen_ai_request_model, gen_ai_response_model,
	input_tokens, output_tokens, total_tokens, has_payload, attributes_json
) VALUES (
	?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
)
"#;

const INSERT_PAYLOAD: &str = r#"
INSERT INTO request_log_payloads (log_id, request_prompt_json, response_completion_json)
VALUES (?, ?, ?)
"#;
