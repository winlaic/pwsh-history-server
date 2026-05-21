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

const DEFAULT_PORT: u16 = 37373;
const PROFILE_BEGIN: &str = "# >>> pwsh-history-server >>>";
const PROFILE_END: &str = "# <<< pwsh-history-server <<<";
const PWSH_HISTORY_SCRIPT: &str = include_str!("../pwsh-history.ps1");

#[derive(Debug)]
struct Config {
    db_path: String,
    token: String,
    port: u16,
    addr: SocketAddr,
    url: String,
    lazy: bool,
    sources: ConfigSources,
}

#[derive(Debug)]
struct ConfigSources {
    db_path: ConfigSource,
    token: ConfigSource,
    port: ConfigSource,
}

#[derive(Debug, Clone, Copy)]
enum ConfigSource {
    Arg,
    Profile,
    Default,
    Generated,
}

#[derive(Debug, Default)]
struct ProfileConfig {
    token: Option<String>,
}

#[derive(Debug, Default)]
struct CliArgs {
    db_path: Option<String>,
    port: Option<u16>,
    token: Option<String>,
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
        let args = parse_args(env::args().skip(1))?;
        let profile_config = load_profile_config();
        let (db_path, db_path_source) =
            resolve_config_value(args.db_path, None, default_db_path, ConfigSource::Default)?;
        let (token, token_source) = resolve_config_value(
            args.token,
            profile_config.token,
            || Ok(generate_token()),
            ConfigSource::Generated,
        )?;
        let (port, port_source) =
            resolve_config_value(args.port, None, || Ok(DEFAULT_PORT), ConfigSource::Default)?;
        let bind = format!("0.0.0.0:{port}");
        let addr: SocketAddr = bind
            .parse()
            .with_context(|| format!("invalid bind address derived from --port: {bind}"))?;
        let url = default_route_url(port);

        Ok(Self {
            db_path,
            token,
            port,
            addr,
            url,
            lazy: args.lazy,
            sources: ConfigSources {
                db_path: db_path_source,
                token: token_source,
                port: port_source,
            },
        })
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliArgs> {
    let mut parsed = CliArgs::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lazy" => parsed.lazy = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--db" => parsed.db_path = Some(required_arg_value("--db", args.next())?),
            "--port" => {
                parsed.port = Some(parse_port(&required_arg_value("--port", args.next())?)?)
            }
            "--token" => parsed.token = Some(required_arg_value("--token", args.next())?),
            _ if arg.starts_with("--db=") => {
                parsed.db_path = Some(non_empty_arg_value("--db", &arg["--db=".len()..])?)
            }
            _ if arg.starts_with("--port=") => {
                parsed.port = Some(parse_port(&arg["--port=".len()..])?)
            }
            _ if arg.starts_with("--token=") => {
                parsed.token = Some(non_empty_arg_value("--token", &arg["--token=".len()..])?)
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }

    Ok(parsed)
}

fn required_arg_value(name: &str, value: Option<String>) -> Result<String> {
    let value = value.with_context(|| format!("{name} requires a value"))?;
    non_empty_arg_value(name, &value)
}

fn non_empty_arg_value(name: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{name} cannot be empty");
    }
    Ok(value.to_string())
}

fn parse_port(value: &str) -> Result<u16> {
    let port: u16 = value
        .trim()
        .parse()
        .with_context(|| format!("invalid --port value: {value}"))?;
    if port == 0 {
        bail!("--port must be between 1 and 65535");
    }
    Ok(port)
}

fn resolve_config_value<T>(
    arg_value: Option<T>,
    profile_value: Option<T>,
    default: impl FnOnce() -> Result<T>,
    default_source: ConfigSource,
) -> Result<(T, ConfigSource)> {
    if let Some(value) = arg_value {
        return Ok((value, ConfigSource::Arg));
    }

    if let Some(value) = profile_value {
        return Ok((value, ConfigSource::Profile));
    }

    Ok((default()?, default_source))
}

fn load_profile_config() -> ProfileConfig {
    let profile_path = current_user_all_hosts_profile()
        .map(PathBuf::from)
        .or_else(|| {
            home_dir()
                .ok()
                .map(|home| home.join(".config").join("powershell").join("profile.ps1"))
        });

    let Some(profile_path) = profile_path else {
        return ProfileConfig::default();
    };

    let Ok(content) = fs::read_to_string(profile_path) else {
        return ProfileConfig::default();
    };

    parse_profile_config(&content)
}

fn parse_profile_config(content: &str) -> ProfileConfig {
    let mut config = ProfileConfig::default();

    for line in content.lines() {
        let Some(value) = parse_profile_token_assignment(line) else {
            continue;
        };
        config.token = Some(value);
    }

    config
}

fn parse_profile_token_assignment(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let value = trimmed.strip_prefix("$env:PWSH_HISTORY_TOKEN")?;
    let (_, value) = value.split_once('=')?;
    let value = parse_ps_single_quoted(value.trim())?;
    if value.trim().is_empty() {
        return None;
    }
    Some(value)
}

fn parse_ps_single_quoted(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'"))
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
    println!("server:");
    println!(
        "  db: {} ({})",
        config.db_path,
        config.sources.db_path.describe("--db")
    );
    println!("  bind: {} (fixed)", config.addr);
    println!(
        "  port: {} ({})",
        config.port,
        config.sources.port.describe("--port")
    );
    println!(
        "  token: {} ({})",
        config.token,
        config.sources.token.describe("--token")
    );
    println!();
    println!("client:");
    println!(
        "  url: {} (derived from current server IP and port)",
        config.url
    );
    println!(
        "  lazy install: {}",
        if config.lazy { "enabled" } else { "disabled" }
    );
}

impl ConfigSource {
    fn describe(self, name: &str) -> String {
        match self {
            ConfigSource::Arg => format!("from {name}"),
            ConfigSource::Profile => "from profile $PROFILE.CurrentUserAllHosts".to_string(),
            ConfigSource::Default => "program default".to_string(),
            ConfigSource::Generated => "generated new random value".to_string(),
        }
    }
}

fn print_help() {
    println!(
        "\
pwsh-history-server

Usage:
  pwsh-history-server [--db PATH] [--port PORT] [--token TOKEN] [--lazy]

Server options:
  --db PATH             SQLite database path
                        default: $HOME/.local/share/pwsh-history/history.sqlite3
  --port PORT           listen on 0.0.0.0:PORT
                        default: {DEFAULT_PORT}
  --token TOKEN         HTTP API token
                        default: profile token, otherwise generated

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
    write_file(&script_path, PWSH_HISTORY_SCRIPT)
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

    write_file(path, content)
}

fn write_file(path: &Path, content: &str) -> Result<()> {
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
    if let Some((start, end)) = managed_profile_block_range(current) {
        let outside = format!("{}{}", &current[..start], &current[end..]);
        let include_source = !outside.lines().any(is_pwsh_history_source_line);
        return format!(
            "{}{}{}",
            &current[..start],
            managed_profile_block(config, script_path, include_source),
            &current[end..]
        );
    }

    if let Some(source_start) = find_pwsh_history_source_line_start(current) {
        return format!(
            "{}{}{}",
            &current[..source_start],
            managed_profile_block(config, script_path, false),
            &current[source_start..]
        );
    }

    let mut output = current.to_string();
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&managed_profile_block(config, script_path, true));
    output
}

fn managed_profile_block_range(content: &str) -> Option<(usize, usize)> {
    let start = content.find(PROFILE_BEGIN)?;
    let end_start = content[start..].find(PROFILE_END)? + start;
    let mut end = end_start + PROFILE_END.len();

    if content[end..].starts_with("\r\n") {
        end += 2;
    } else if content[end..].starts_with('\n') {
        end += 1;
    }

    Some((start, end))
}

fn find_pwsh_history_source_line_start(content: &str) -> Option<usize> {
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if is_pwsh_history_source_line(line.trim_end_matches(['\r', '\n'])) {
            return Some(offset);
        }
        offset += line.len();
    }

    if !content.ends_with('\n') {
        let line_start = content.rfind('\n').map_or(0, |index| index + 1);
        if is_pwsh_history_source_line(&content[line_start..]) {
            return Some(line_start);
        }
    }

    None
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
        "$env:PWSH_HISTORY_TOKEN = '{}'\n",
        ps_single_quote(&config.token)
    ));
    block.push_str(&format!(
        "$env:PWSH_HISTORY_URL = '{}'\n",
        ps_single_quote(&config.url)
    ));
    if include_source {
        block.push_str(&format!(". {}\n", ps_path_expr(script_path)));
    }
    block.push_str(PROFILE_END);
    block.push('\n');
    block
}

fn ps_path_expr(path: &Path) -> String {
    if let Ok(home) = env::var("HOME") {
        return ps_path_expr_with_home(path, Path::new(&home));
    }
    format!("'{}'", ps_single_quote(&path.to_string_lossy()))
}

fn ps_path_expr_with_home(path: &Path, home: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(home) {
        let relative = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if !relative.is_empty() {
            return format!("(Join-Path $HOME '{}')", ps_single_quote(&relative));
        }
    }

    format!("'{}'", ps_single_quote(&path.to_string_lossy()))
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
    let result = sqlx::query(
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

    info!(
        command_len = command.len(),
        rows_affected = result.rows_affected(),
        "history add"
    );

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

    info!(
        prefix = %query.prefix,
        limit,
        hits = entries.len(),
        "history search"
    );

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
        Config, ConfigSource, ConfigSources, DEFAULT_PORT, escape_like, managed_profile_block,
        parse_args, parse_profile_config, ps_path_expr_with_home, update_profile_content,
    };
    use std::{net::SocketAddr, path::Path};

    #[test]
    fn escape_like_escapes_sqlite_wildcards() {
        assert_eq!(escape_like(r"git_%\branch"), r"git\_\%\\branch");
    }

    #[test]
    fn default_port_is_37373() {
        assert_eq!(DEFAULT_PORT, 37373);
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
        assert!(!block.contains("PWSH_HISTORY_DB"));
        assert!(block.contains("$env:PWSH_HISTORY_TOKEN = 'env-token'"));
        assert!(!block.contains("PWSH_HISTORY_BIND"));
        assert!(block.contains("$env:PWSH_HISTORY_URL = 'http://history-host:9999'"));
    }

    #[test]
    fn powershell_path_expression_prefers_home_relative_paths() {
        assert_eq!(
            ps_path_expr_with_home(
                Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
                Path::new("/home/me"),
            ),
            "(Join-Path $HOME '.config/powershell/pwsh-history.ps1')"
        );
        assert_eq!(
            ps_path_expr_with_home(Path::new("/opt/pwsh-history.ps1"), Path::new("/home/me")),
            "'/opt/pwsh-history.ps1'"
        );
    }

    #[test]
    fn profile_config_reads_only_existing_token() {
        let profile = "\
# >>> pwsh-history-server >>>
$env:PWSH_HISTORY_DB = (Join-Path $HOME '.local/share/pwsh-history/history.sqlite3')
$env:PWSH_HISTORY_TOKEN = 'keep-this-token'
$env:PWSH_HISTORY_URL = 'http://10.0.0.2:37373'
# <<< pwsh-history-server <<<
";
        let config = parse_profile_config(profile);

        assert_eq!(config.token.as_deref(), Some("keep-this-token"));
    }

    #[test]
    fn profile_config_reads_escaped_token_and_ignores_other_fields() {
        let profile = "\
$env:PWSH_HISTORY_TOKEN = 'it''s-token'
$env:PWSH_HISTORY_BIND = '0.0.0.0:38444'
";
        let config = parse_profile_config(profile);

        assert_eq!(config.token.as_deref(), Some("it's-token"));
    }

    #[test]
    fn parse_args_accepts_server_options() {
        let args = parse_args([
            "--db".to_string(),
            "/tmp/history.sqlite3".to_string(),
            "--port=38444".to_string(),
            "--token".to_string(),
            "token".to_string(),
            "--lazy".to_string(),
        ])
        .unwrap();

        assert_eq!(args.db_path.as_deref(), Some("/tmp/history.sqlite3"));
        assert_eq!(args.port, Some(38444));
        assert_eq!(args.token.as_deref(), Some("token"));
        assert!(args.lazy);
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
    fn profile_update_is_stable_without_growing_blank_lines() {
        let config = test_config();
        let current = "\
before

# >>> pwsh-history-server >>>
$env:PWSH_HISTORY_TOKEN = 'old'
# <<< pwsh-history-server <<<
";

        let once = update_profile_content(
            current,
            &config,
            Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
        );
        let twice = update_profile_content(
            &once,
            &config,
            Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
        );

        assert_eq!(once, twice);
        assert!(!once.contains("before\n\n\n# >>> pwsh-history-server >>>"));
        assert!(once.contains("before\n\n# >>> pwsh-history-server >>>"));
    }

    #[test]
    fn profile_update_preserves_user_whitespace_around_managed_block() {
        let config = test_config();
        let current = "\
before


# >>> pwsh-history-server >>>
$env:PWSH_HISTORY_TOKEN = 'old'
# <<< pwsh-history-server <<<


after
";

        let updated = update_profile_content(
            current,
            &config,
            Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
        );
        let twice = update_profile_content(
            &updated,
            &config,
            Path::new("/home/me/.config/powershell/pwsh-history.ps1"),
        );

        assert_eq!(updated, twice);
        assert!(updated.contains("before\n\n\n# >>> pwsh-history-server >>>"));
        assert!(updated.contains("# <<< pwsh-history-server <<<\n\n\nafter"));
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

    fn test_config() -> Config {
        Config {
            db_path: "/tmp/env.sqlite3".to_string(),
            token: "env-token".to_string(),
            port: 9999,
            addr: "0.0.0.0:9999".parse::<SocketAddr>().unwrap(),
            url: "http://history-host:9999".to_string(),
            lazy: true,
            sources: ConfigSources {
                db_path: ConfigSource::Arg,
                token: ConfigSource::Arg,
                port: ConfigSource::Arg,
            },
        }
    }
}
