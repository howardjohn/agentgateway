use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::types::Json;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use super::{
	GenAiEntry, GetRequest, GetResponse, GroupBy, GroupByField, LogEntry, LogFilters, PayloadEntry,
	SearchRequest, SearchResponse, StoredRequestLog, TimeRange, TokenUsageGroup, TokenUsageRequest,
	TokenUsageResponse, UsageEntry, attr_value, decode_cursor, encode_cursor, limit,
};

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

	pub async fn search(&self, request: SearchRequest) -> anyhow::Result<SearchResponse> {
		let limit = limit(request.limit);
		let mut qb = QueryBuilder::<Sqlite>::new(format!("{SELECT_LOGS} WHERE 1=1"));
		push_filters(&mut qb, request.time_range.as_ref(), &request.filters);
		if let Some(cursor) = request.cursor.as_deref() {
			let (completed_at, id) = decode_cursor(cursor)?;
			qb.push(" AND (completed_at < ");
			qb.push_bind(completed_at);
			qb.push(" OR (completed_at = ");
			qb.push_bind(completed_at);
			qb.push(" AND id < ");
			qb.push_bind(id);
			qb.push("))");
		}
		qb.push(" ORDER BY completed_at DESC, id DESC LIMIT ");
		qb.push_bind(limit + 1);
		let rows = qb.build().fetch_all(&self.pool).await?;
		let mut logs = rows
			.into_iter()
			.map(|row| row_to_log(row, request.include_attributes, false))
			.collect::<Result<Vec<_>, _>>()?;
		let next_cursor = if logs.len() > limit as usize {
			let _ = logs.pop();
			logs
				.last()
				.map(|log| encode_cursor(log.completed_at, &log.id))
		} else {
			None
		};
		Ok(SearchResponse { logs, next_cursor })
	}

	pub async fn get(&self, request: GetRequest) -> anyhow::Result<GetResponse> {
		let row = if request.include_payload {
			sqlx::query(SELECT_LOG_WITH_PAYLOAD_BY_ID)
				.bind(request.id)
				.fetch_optional(&self.pool)
				.await?
		} else {
			sqlx::query(SELECT_LOG_BY_ID)
				.bind(request.id)
				.fetch_optional(&self.pool)
				.await?
		};
		let log = row
			.map(|row| row_to_log(row, true, request.include_payload))
			.transpose()?;
		Ok(GetResponse { log })
	}

	pub async fn token_usage(
		&self,
		request: TokenUsageRequest,
	) -> anyhow::Result<TokenUsageResponse> {
		let mut qb = QueryBuilder::<Sqlite>::new("SELECT ");
		push_group_select(&mut qb, &request.group_by);
		if !request.group_by.is_empty() {
			qb.push(", ");
		}
		qb.push("COUNT(*) AS requests, COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, COALESCE(SUM(total_tokens), 0) AS total_tokens FROM request_logs WHERE 1=1");
		push_filters(&mut qb, request.time_range.as_ref(), &request.filters);
		if !request.group_by.is_empty() {
			qb.push(" GROUP BY ");
			let mut separated = qb.separated(", ");
			for idx in 0..request.group_by.len() {
				separated.push(format!("g{idx}"));
			}
		}
		let rows = qb.build().fetch_all(&self.pool).await?;
		let groups = rows
			.into_iter()
			.map(|row| row_to_token_usage(row, &request.group_by))
			.collect::<Result<Vec<_>, _>>()?;
		Ok(TokenUsageResponse { groups })
	}
}

fn push_filters(
	qb: &mut QueryBuilder<Sqlite>,
	time_range: Option<&TimeRange>,
	filters: &LogFilters,
) {
	if let Some(from) = time_range.and_then(|r| r.from) {
		qb.push(" AND completed_at >= ");
		qb.push_bind(from);
	}
	if let Some(to) = time_range.and_then(|r| r.to) {
		qb.push(" AND completed_at < ");
		qb.push_bind(to);
	}
	push_in(qb, "http_status", &filters.http_status);
	push_in(qb, "gen_ai_provider_name", &filters.provider);
	push_in(qb, "gen_ai_request_model", &filters.request_model);
	push_in(qb, "gen_ai_response_model", &filters.response_model);
	if let Some(trace_id) = &filters.trace_id {
		qb.push(" AND trace_id = ");
		qb.push_bind(trace_id);
	}
	if let Some(has_payload) = filters.has_payload {
		qb.push(" AND has_payload = ");
		qb.push_bind(has_payload);
	}
	for (key, value) in &filters.attributes {
		let Some(value) = attr_value(value) else {
			qb.push(" AND 1=0");
			continue;
		};
		qb.push(" AND CAST(json_extract(attributes_json, ");
		qb.push_bind(json_path(key));
		qb.push(") AS TEXT) = ");
		qb.push_bind(sqlite_attr_value(value));
	}
}

fn push_in<T>(qb: &mut QueryBuilder<Sqlite>, column: &str, values: &[T])
where
	T: for<'q> sqlx::Encode<'q, Sqlite> + sqlx::Type<Sqlite> + Send + Sync,
{
	if values.is_empty() {
		return;
	}
	qb.push(" AND ");
	qb.push(column);
	qb.push(" IN (");
	let mut separated = qb.separated(", ");
	for value in values {
		separated.push_bind(value);
	}
	separated.push_unseparated(")");
}

fn push_group_select(qb: &mut QueryBuilder<Sqlite>, group_by: &[GroupBy]) {
	let mut separated = qb.separated(", ");
	for (idx, group) in group_by.iter().enumerate() {
		match group.field {
			GroupByField::Provider => {
				separated.push(format!("gen_ai_provider_name AS g{idx}"));
			},
			GroupByField::RequestModel => {
				separated.push(format!("gen_ai_request_model AS g{idx}"));
			},
			GroupByField::ResponseModel => {
				separated.push(format!("gen_ai_response_model AS g{idx}"));
			},
			GroupByField::HttpStatus => {
				separated.push(format!("CAST(http_status AS TEXT) AS g{idx}"));
			},
			GroupByField::Attributes => {
				separated.push("CAST(json_extract(attributes_json, ");
				separated.push_bind(json_path(group.key.as_deref().unwrap_or_default()));
				separated.push(format!(") AS TEXT) AS g{idx}"));
			},
		};
	}
}

fn row_to_token_usage(
	row: sqlx::sqlite::SqliteRow,
	group_by: &[GroupBy],
) -> anyhow::Result<TokenUsageGroup> {
	let mut group = std::collections::BTreeMap::new();
	for (idx, spec) in group_by.iter().enumerate() {
		let value: Option<String> = row.try_get(format!("g{idx}").as_str())?;
		group.insert(
			group_key(spec),
			value.map(Value::String).unwrap_or(Value::Null),
		);
	}
	Ok(TokenUsageGroup {
		group,
		requests: row.try_get("requests")?,
		input_tokens: row.try_get("input_tokens")?,
		output_tokens: row.try_get("output_tokens")?,
		total_tokens: row.try_get("total_tokens")?,
	})
}

fn row_to_log(
	row: sqlx::sqlite::SqliteRow,
	include_attributes: bool,
	include_payload: bool,
) -> anyhow::Result<LogEntry> {
	let attributes: Json<Value> = row.try_get("attributes_json")?;
	let payload = if include_payload {
		let request_prompt: Option<Json<Value>> = row.try_get("request_prompt_json")?;
		let response_completion: Option<Json<Value>> = row.try_get("response_completion_json")?;
		Some(PayloadEntry {
			request_prompt: request_prompt.map(|v| v.0),
			response_completion: response_completion.map(|v| v.0),
		})
	} else {
		None
	};
	Ok(LogEntry {
		id: row.try_get("id")?,
		started_at: row.try_get("started_at")?,
		completed_at: row.try_get("completed_at")?,
		duration_ms: row.try_get("duration_ms")?,
		trace_id: row.try_get("trace_id")?,
		span_id: row.try_get("span_id")?,
		http_status: row.try_get("http_status")?,
		error: row.try_get("error")?,
		gen_ai: GenAiEntry {
			operation_name: row.try_get("gen_ai_operation_name")?,
			provider_name: row.try_get("gen_ai_provider_name")?,
			request_model: row.try_get("gen_ai_request_model")?,
			response_model: row.try_get("gen_ai_response_model")?,
		},
		usage: UsageEntry {
			input_tokens: row.try_get("input_tokens")?,
			output_tokens: row.try_get("output_tokens")?,
			total_tokens: row.try_get("total_tokens")?,
		},
		has_payload: row.try_get("has_payload")?,
		attributes: include_attributes.then_some(attributes.0),
		payload,
	})
}

fn group_key(group: &GroupBy) -> String {
	match group.field {
		GroupByField::Provider => "provider".to_string(),
		GroupByField::RequestModel => "requestModel".to_string(),
		GroupByField::ResponseModel => "responseModel".to_string(),
		GroupByField::HttpStatus => "httpStatus".to_string(),
		GroupByField::Attributes => group
			.key
			.clone()
			.unwrap_or_else(|| "attributes".to_string()),
	}
}

fn json_path(key: &str) -> String {
	format!(
		"$.{}",
		key
			.split('.')
			.map(|part| format!("\"{part}\""))
			.collect::<Vec<_>>()
			.join(".")
	)
}

fn sqlite_attr_value(value: String) -> String {
	match value.as_str() {
		"true" => "1".to_string(),
		"false" => "0".to_string(),
		_ => value,
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

const SELECT_LOGS: &str = r#"
SELECT id, started_at, completed_at, duration_ms, trace_id, span_id, http_status, error,
	gen_ai_operation_name, gen_ai_provider_name, gen_ai_request_model, gen_ai_response_model,
	input_tokens, output_tokens, total_tokens, has_payload, attributes_json
FROM request_logs
"#;

const SELECT_LOG_BY_ID: &str = r#"
SELECT id, started_at, completed_at, duration_ms, trace_id, span_id, http_status, error,
	gen_ai_operation_name, gen_ai_provider_name, gen_ai_request_model, gen_ai_response_model,
	input_tokens, output_tokens, total_tokens, has_payload, attributes_json
FROM request_logs
WHERE request_logs.id = ?
"#;

const SELECT_LOG_WITH_PAYLOAD_BY_ID: &str = r#"
SELECT request_logs.id, started_at, completed_at, duration_ms, trace_id, span_id, http_status, error,
	gen_ai_operation_name, gen_ai_provider_name, gen_ai_request_model, gen_ai_response_model,
	input_tokens, output_tokens, total_tokens, has_payload, attributes_json,
	request_prompt_json, response_completion_json
FROM request_logs
LEFT JOIN request_log_payloads ON request_logs.id = request_log_payloads.log_id
WHERE request_logs.id = ?
"#;
