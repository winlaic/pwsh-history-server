use std::{
    env, fs,
    io::Write,
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    process::Command,
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

const DEFAULT_BIND: &str = "0.0.0.0:37373";
const PROFILE_BEGIN: &str = "# >>> pwsh-history-server >>>";
const PROFILE_END: &str = "# <<< pwsh-history-server <<<";
const PWSH_HISTORY_SCRIPT: &str = include_str!("../pwsh-history.ps1");

#[derive(Debug)]
struct Config {
    db_path: String,
    token: String,
    bind: String,
    addr: SocketAddr,
    url: String,
    lazy: bool,
}

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

    let config = Config::from_env_and_args()?;
    print_startup_config(&config);

    if config.lazy {
        install_lazy_profile(&config)?;
    }

    if let Some(parent) = Path::new(&config.db_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create database directory {}", parent.display()))?;
    }

    let db = open_database(&config.db_path).await?;
    migrate(&db).await?;

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/history/add", post(add_history))
        .route("/v1/history/search", get(search_history))
        .with_state(AppState {
            db,
            token: config.token,
        });

    let listener = TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("failed to bind {}", config.addr))?;

    info!(addr = %config.addr, db_path = %config.db_path, "pwsh history server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

impl Config {
    fn from_env_and_args() -> Result<Self> {
        let mut lazy = false;
        for arg in env::args().skip(1) {
            match arg.as_str() {
                "--lazy" => lazy = true,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }

        let db_path = optional_env("PWSH_HISTORY_DB")
            .map(Ok)
            .unwrap_or_else(default_db_path)?;
        let token = optional_env("PWSH_HISTORY_TOKEN").unwrap_or_else(generate_token);
        let bind = optional_env("PWSH_HISTORY_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let addr: SocketAddr = bind
            .parse()
            .with_context(|| format!("invalid PWSH_HISTORY_BIND: {bind}"))?;
        let url = optional_env("PWSH_HISTORY_URL").unwrap_or_else(|| default_url_for_addr(addr));

        Ok(Self {
            db_path,
            token,
            bind,
            addr,
            url,
            lazy,
        })
    }
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_db_path() -> Result<String> {
    Ok(home_dir()?
        .join(".local")
        .join("share")
        .join("pwsh-history")
        .join("history.sqlite3")
        .to_string_lossy()
        .into_owned())
}

fn home_dir() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    if home.trim().is_empty() {
        bail!("HOME is empty");
    }
    Ok(PathBuf::from(home))
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("failed to get random bytes for PWSH_HISTORY_TOKEN");
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn default_url_for_addr(addr: SocketAddr) -> String {
    let port = addr.port();
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => default_route_url(port),
        IpAddr::V6(ip) if ip.is_unspecified() => default_route_url(port),
        IpAddr::V6(ip) => format!("http://[{ip}]:{port}"),
        ip => format!("http://{ip}:{port}"),
    }
}

fn default_route_url(port: u16) -> String {
    match default_route_ip() {
        Some(IpAddr::V6(ip)) => format!("http://[{ip}]:{port}"),
        Some(ip) => format!("http://{ip}:{port}"),
        None => format!("http://0.0.0.0:{port}"),
    }
}

fn default_route_ip() -> Option<IpAddr> {
    default_route_ip_with("0.0.0.0:0", "8.8.8.8:80")
        .or_else(|| default_route_ip_with("[::]:0", "[2001:4860:4860::8888]:80"))
}

fn default_route_ip_with(bind: &str, destination: &str) -> Option<IpAddr> {
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(destination).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn print_startup_config(config: &Config) {
    println!("PWSH_HISTORY_DB={}", config.db_path);
    println!("PWSH_HISTORY_TOKEN={}", config.token);
    println!("PWSH_HISTORY_BIND={}", config.bind);
    println!("PWSH_HISTORY_URL={}", config.url);
    if config.lazy {
        println!("PWSH_HISTORY_LAZY=1");
    }
}

fn print_help() {
    println!(
        "\
pwsh-history-server

Usage:
  pwsh-history-server [--lazy]

Environment overrides:
  PWSH_HISTORY_DB       default: $HOME/.local/share/pwsh-history/history.sqlite3
  PWSH_HISTORY_TOKEN    default: random 32-byte hex token printed at startup
  PWSH_HISTORY_BIND     default: {DEFAULT_BIND}
  PWSH_HISTORY_URL      default: default-route IP derived from bind port

Options:
  --lazy                install pwsh-history.ps1 and update $PROFILE.CurrentUserAllHosts
  -h, --help            show this help
"
    );
}

fn install_lazy_profile(config: &Config) -> Result<()> {
    let powershell_dir = home_dir()?.join(".config").join("powershell");
    fs::create_dir_all(&powershell_dir).with_context(|| {
        format!(
            "failed to create PowerShell config directory {}",
            powershell_dir.display()
        )
    })?;

    let script_path = powershell_dir.join("pwsh-history.ps1");
    write_file_if_changed(&script_path, PWSH_HISTORY_SCRIPT)
        .with_context(|| format!("failed to write {}", script_path.display()))?;

    let profile_path = current_user_all_hosts_profile().unwrap_or_else(|| {
        powershell_dir
            .join("profile.ps1")
            .to_string_lossy()
            .into_owned()
    });
    let profile_path = PathBuf::from(profile_path);
    if let Some(parent) = profile_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create profile directory {}", parent.display()))?;
    }

    let old_profile = fs::read_to_string(&profile_path).unwrap_or_default();
    let new_profile = update_profile_content(&old_profile, config, &script_path);
    write_file_if_changed(&profile_path, &new_profile)
        .with_context(|| format!("failed to update {}", profile_path.display()))?;

    println!(
        "Installed PowerShell history client: {}",
        script_path.display()
    );
    println!(
        "Updated PowerShell profile: {}",
        profile_path.to_string_lossy()
    );

    Ok(())
}

fn write_file_if_changed(path: &Path, content: &str) -> Result<()> {
    if fs::read_to_string(path).is_ok_and(|current| current == content) {
        return Ok(());
    }

    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

fn current_user_all_hosts_profile() -> Option<String> {
    let output = Command::new("pwsh")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "$PROFILE.CurrentUserAllHosts",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

fn update_profile_content(current: &str, config: &Config, script_path: &Path) -> String {
    let cleaned = remove_managed_profile_blocks(current);
    let existing_source = cleaned.lines().position(is_pwsh_history_source_line);

    let mut output = String::new();
    match existing_source {
        Some(source_line) => {
            for (line_index, line) in cleaned.lines().enumerate() {
                if line_index == source_line {
                    output.push_str(&managed_profile_block(config, script_path, false));
                }
                output.push_str(line);
                output.push('\n');
            }
        }
        None => {
            if !cleaned.trim().is_empty() {
                output.push_str(&cleaned);
                if !cleaned.ends_with('\n') {
                    output.push('\n');
                }
                output.push('\n');
            }
            output.push_str(&managed_profile_block(config, script_path, true));
        }
    }

    output
}

fn remove_managed_profile_blocks(current: &str) -> String {
    let mut output = Vec::new();
    let mut in_block = false;

    for line in current.lines() {
        if line.trim() == PROFILE_BEGIN {
            in_block = true;
            continue;
        }

        if line.trim() == PROFILE_END {
            in_block = false;
            continue;
        }

        if !in_block {
            output.push(line);
        }
    }

    let mut cleaned = output.join("\n");
    if current.ends_with('\n') && !cleaned.is_empty() {
        cleaned.push('\n');
    }
    cleaned
}

fn is_pwsh_history_source_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let is_dot_source = trimmed
        .strip_prefix('.')
        .is_some_and(|rest| rest.starts_with(char::is_whitespace));

    is_dot_source && trimmed.to_ascii_lowercase().contains("pwsh-history.ps1")
}

fn managed_profile_block(config: &Config, script_path: &Path, include_source: bool) -> String {
    let mut block = String::new();
    block.push_str(PROFILE_BEGIN);
    block.push('\n');
    block.push_str(&format!(
        "$env:PWSH_HISTORY_DB = '{}'\n",
        ps_single_quote(&config.db_path)
    ));
    block.push_str(&format!(
        "$env:PWSH_HISTORY_TOKEN = '{}'\n",
        ps_single_quote(&config.token)
    ));
    block.push_str(&format!(
        "$env:PWSH_HISTORY_BIND = '{}'\n",
        ps_single_quote(&config.bind)
    ));
    block.push_str(&format!(
        "$env:PWSH_HISTORY_URL = '{}'\n",
        ps_single_quote(&config.url)
    ));
    if include_source {
        block.push_str(&format!(
            ". '{}'\n",
            ps_single_quote(&script_path.to_string_lossy())
        ));
    }
    block.push_str(PROFILE_END);
    block.push('\n');
    block
}

fn ps_single_quote(value: &str) -> String {
    value.replace('\'', "''")
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
    use super::{
        Config, DEFAULT_BIND, escape_like, managed_profile_block, remove_managed_profile_blocks,
        update_profile_content,
    };
    use std::{net::SocketAddr, path::Path};

    #[test]
    fn escape_like_escapes_sqlite_wildcards() {
        assert_eq!(escape_like(r"git_%\branch"), r"git\_\%\\branch");
    }

    #[test]
    fn default_bind_is_all_interfaces() {
        assert_eq!(DEFAULT_BIND, "0.0.0.0:37373");
    }

    #[test]
    fn managed_profile_block_writes_effective_environment_before_source() {
        let config = test_config();
        let block = managed_profile_block(
            &config,
            Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
            true,
        );
        let token_index = block.find("$env:PWSH_HISTORY_TOKEN").unwrap();
        let source_index = block
            .find(". '/home/me/.config/powershell/pwsh-history.ps1'")
            .unwrap();

        assert!(token_index < source_index);
        assert!(block.contains("$env:PWSH_HISTORY_DB = '/tmp/env.sqlite3'"));
        assert!(block.contains("$env:PWSH_HISTORY_TOKEN = 'env-token'"));
        assert!(block.contains("$env:PWSH_HISTORY_BIND = '1.2.3.4:9999'"));
        assert!(block.contains("$env:PWSH_HISTORY_URL = 'http://history-host:9999'"));
    }

    #[test]
    fn profile_update_replaces_existing_managed_block() {
        let config = test_config();
        let current = "\
before
# >>> pwsh-history-server >>>
$env:PWSH_HISTORY_TOKEN = 'old'
. '/old/pwsh-history.ps1'
# <<< pwsh-history-server <<<
after
";

        let updated = update_profile_content(
            current,
            &config,
            Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
        );

        assert!(updated.contains("before"));
        assert!(updated.contains("after"));
        assert!(!updated.contains("old"));
        assert_eq!(updated.matches("# >>> pwsh-history-server >>>").count(), 1);
        assert!(updated.contains(". '/home/me/.config/powershell/pwsh-history.ps1'"));
    }

    #[test]
    fn profile_update_places_env_block_before_existing_source() {
        let config = test_config();
        let current = "Write-Host hi\n. '/custom/pwsh-history.ps1'\n";
        let updated = update_profile_content(
            current,
            &config,
            Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
        );

        let token_index = updated.find("$env:PWSH_HISTORY_TOKEN").unwrap();
        let source_index = updated.find(". '/custom/pwsh-history.ps1'").unwrap();

        assert!(token_index < source_index);
        assert!(updated.contains(". '/custom/pwsh-history.ps1'"));
        assert!(!updated.contains(". '/home/me/.config/powershell/pwsh-history.ps1'"));
    }

    #[test]
    fn profile_update_does_not_treat_comments_as_source() {
        let config = test_config();
        let current = "# source pwsh-history.ps1 later\n";
        let updated = update_profile_content(
            current,
            &config,
            Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
        );

        assert!(updated.contains("# source pwsh-history.ps1 later"));
        assert!(updated.contains(". '/home/me/.config/powershell/pwsh-history.ps1'"));
    }

    #[test]
    fn remove_managed_profile_blocks_keeps_unmanaged_content() {
        let current = "\
one
# >>> pwsh-history-server >>>
managed
# <<< pwsh-history-server <<<
two
";
        assert_eq!(remove_managed_profile_blocks(current), "one\ntwo\n");
    }

    fn test_config() -> Config {
        Config {
            db_path: "/tmp/env.sqlite3".to_string(),
            token: "env-token".to_string(),
            bind: "1.2.3.4:9999".to_string(),
            addr: "1.2.3.4:9999".parse::<SocketAddr>().unwrap(),
            url: "http://history-host:9999".to_string(),
            lazy: true,
        }
    }
}
