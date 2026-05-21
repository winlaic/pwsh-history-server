use std::{
    env,
    net::SocketAddr,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use tokio::net::TcpListener;
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    token: String,
}

#[derive(Debug, Deserialize)]
struct AddHistoryRequest {
    command: String,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    prefix: String,
    limit: Option<i64>,
}

#[derive(Debug, Serialize, FromRow)]
struct HistoryEntry {
    id: i64,
    command: String,
    last_seen_at: i64,
    use_count: i64,
}

#[derive(Debug, Serialize)]
struct AddHistoryResponse {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct SearchResponse {
    entries: Vec<HistoryEntry>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pwsh_history_server=info,tower_http=info".into()),
        )
        .init();

    let db_path = required_env("PWSH_HISTORY_DB")?;
    let token = required_env("PWSH_HISTORY_TOKEN")?;
    let bind = required_env("PWSH_HISTORY_BIND")?;
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid PWSH_HISTORY_BIND: {bind}"))?;

    if let Some(parent) = Path::new(&db_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create database directory {}", parent.display()))?;
    }

    let db = open_database(&db_path).await?;
    migrate(&db).await?;

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/history/add", post(add_history))
        .route("/v1/history/search", get(search_history))
        .with_state(AppState { db, token });

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    info!(%addr, db_path, "pwsh history server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("{name} is not set"))?;
    if value.trim().is_empty() {
        bail!("{name} is empty");
    }
    Ok(value)
}

async fn open_database(path: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .with_context(|| format!("failed to open sqlite database {path}"))
}

async fn migrate(db: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            command TEXT NOT NULL UNIQUE,
            first_seen_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            use_count INTEGER NOT NULL DEFAULT 1
        );
        "#,
    )
    .execute(db)
    .await
    .context("failed to create history table")?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_history_command
        ON history(command);
        "#,
    )
    .execute(db)
    .await
    .context("failed to create command index")?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_history_last_seen
        ON history(last_seen_at DESC, id DESC);
        "#,
    )
    .execute(db)
    .await
    .context("failed to create last_seen index")?;

    Ok(())
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn add_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    require_auth(&headers, &state)?;

    let command = parse_add_body(&headers, body)?;
    let command = command.trim();
    if command.is_empty() {
        return Err(ApiError::bad_request("command is empty"));
    }

    let now = now_millis()?;
    sqlx::query(
        r#"
        INSERT INTO history(command, first_seen_at, last_seen_at, use_count)
        VALUES (?1, ?2, ?2, 1)
        ON CONFLICT(command) DO UPDATE SET
            last_seen_at = excluded.last_seen_at,
            use_count = history.use_count + 1
        "#,
    )
    .bind(command)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(ApiError::internal)?;

    Ok((StatusCode::OK, Json(AddHistoryResponse { ok: true })))
}

async fn search_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_auth(&headers, &state)?;

    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let pattern = format!("{}%", escape_like(&query.prefix));

    let entries = sqlx::query_as::<_, HistoryEntry>(
        r#"
        SELECT id, command, last_seen_at, use_count
        FROM history
        WHERE command LIKE ?1 ESCAPE '\'
        ORDER BY last_seen_at DESC, id DESC
        LIMIT ?2
        "#,
    )
    .bind(pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(ApiError::internal)?;

    Ok((StatusCode::OK, Json(SearchResponse { entries })))
}

fn parse_add_body(headers: &HeaderMap, body: Bytes) -> Result<String, ApiError> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    if content_type.starts_with("application/json") {
        let request: AddHistoryRequest = serde_json::from_slice(&body)
            .map_err(|error| ApiError::bad_request(format!("invalid json body: {error}")))?;
        return Ok(request.command);
    }

    String::from_utf8(body.to_vec())
        .map_err(|error| ApiError::bad_request(format!("request body is not utf-8: {error}")))
}

fn require_auth(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    let custom = headers
        .get("x-pwsh-history-token")
        .and_then(|value| value.to_str().ok());

    if bearer == Some(state.token.as_str()) || custom == Some(state.token.as_str()) {
        return Ok(());
    }

    Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized"))
}

fn now_millis() -> Result<i64, ApiError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(ApiError::internal)?;
    Ok(duration.as_millis() as i64)
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' | '%' | '_' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        warn!(%error, "failed to listen for shutdown signal");
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::escape_like;

    #[test]
    fn escape_like_escapes_sqlite_wildcards() {
        assert_eq!(escape_like(r"git_%\branch"), r"git\_\%\\branch");
    }
}
