use anyhow::Context;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;

use super::StoredRequestLog;

pub struct PostgresLogStore {
	pool: PgPool,
}

impl PostgresLogStore {
	pub async fn connect(url: &str) -> anyhow::Result<Self> {
		let pool = PgPoolOptions::new()
			.max_connections(5)
			.connect(url)
			.await
			.context("failed to connect request log postgres database")?;
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
	started_at TIMESTAMPTZ NOT NULL,
	completed_at TIMESTAMPTZ NOT NULL,
	duration_ms BIGINT NOT NULL,
	trace_id TEXT,
	span_id TEXT,
	http_status INTEGER,
	error TEXT,
	gen_ai_operation_name TEXT,
	gen_ai_provider_name TEXT,
	gen_ai_request_model TEXT,
	gen_ai_response_model TEXT,
	input_tokens BIGINT,
	output_tokens BIGINT,
	total_tokens BIGINT,
	has_payload BOOLEAN NOT NULL,
	attributes_json JSONB NOT NULL
);

CREATE TABLE IF NOT EXISTS request_log_payloads (
	log_id TEXT PRIMARY KEY REFERENCES request_logs(id) ON DELETE CASCADE,
	request_prompt_json JSONB,
	response_completion_json JSONB
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
	$1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17
)
"#;

const INSERT_PAYLOAD: &str = r#"
INSERT INTO request_log_payloads (log_id, request_prompt_json, response_completion_json)
VALUES ($1, $2, $3)
"#;
