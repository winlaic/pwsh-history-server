# pwsh-history-server

Centralized PowerShell history storage for Linux `pwsh` sessions that share a home directory.

The server is a small Rust HTTP service backed by SQLite. Each PowerShell process records and searches history through the server instead of racing on the same PSReadLine history file.

## Quick start

Build and run:

```sh
cargo build --release
./target/release/pwsh-history-server
```

With no options, the server uses:

```text
db=$HOME/.local/share/pwsh-history/history.sqlite3
port=37373
bind=0.0.0.0:37373
token=<token from PowerShell profile, otherwise random 32-byte hex token printed at startup>
client url=http://<default-route-ip>:37373
```

The server creates the SQLite parent directory automatically and enables WAL mode.

## Configuration

Server configuration is command-line only:

```sh
./target/release/pwsh-history-server \
  --db "$HOME/.local/share/pwsh-history/history.sqlite3" \
  --port 37373 \
  --token "change-this-long-random-token"
```

The server does not read `PWSH_HISTORY_*` environment variables. Those variables are only for the PowerShell client.

`--port` always binds `0.0.0.0:<port>`. The bind address is not configurable.

The token is resolved in this order:

```text
--token > existing PWSH_HISTORY_TOKEN in $PROFILE.CurrentUserCurrentHost > generated random token
```

## Lazy PowerShell setup

Run the server with `--install` on a machine where `pwsh` is installed:

```sh
./target/release/pwsh-history-server --install
```

This copies `pwsh-history.ps1` to:

```text
$HOME/.config/powershell/pwsh-history.ps1
```

Then it updates `$PROFILE.CurrentUserCurrentHost` with a managed block containing the effective client settings before sourcing `pwsh-history.ps1`. The block writes only `PWSH_HISTORY_TOKEN` and `PWSH_HISTORY_URL`. It does not write server-only settings such as the DB path or bind address. If the managed block already exists, only that marked range is replaced; content and whitespace before and after it are preserved exactly.

## HTTP API

All history APIs require either:

```text
Authorization: Bearer <token>
```

or:

```text
X-Pwsh-History-Token: <token>
```

Add a command:

```sh
curl -X POST http://127.0.0.1:37373/v1/history/add \
  -H "X-Pwsh-History-Token: $PWSH_HISTORY_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"command":"git status","cwd":"/absolute/resolved/project/path"}'
```

`cwd` is optional for backward compatibility. The PowerShell client sends the current provider path as a stable absolute directory and resolves symlinks before storing it.

Search by prefix:

```sh
curl "http://127.0.0.1:37373/v1/history/search?prefix=git&limit=100" \
  -H "X-Pwsh-History-Token: $PWSH_HISTORY_TOKEN"
```

Directory-scoped search by prefix:

```sh
curl "http://127.0.0.1:37373/v1/history/search?prefix=git&limit=100&cwd=/absolute/resolved/project/path" \
  -H "X-Pwsh-History-Token: $PWSH_HISTORY_TOKEN"
```

Response:

```json
{
  "entries": [
    {
      "id": 1,
      "command": "git status",
      "cwd": "/absolute/resolved/project/path",
      "last_seen_at": 1779330000000,
      "use_count": 1
    }
  ]
}
```

## PowerShell client

On every client machine, set:

```sh
export PWSH_HISTORY_URL="http://history-server-host:37373"
export PWSH_HISTORY_TOKEN="change-this-long-random-token"
```

Then source the script from your PowerShell profile:

```powershell
. "/path/to/pwsh-history.ps1"
```

The script configures these PSReadLine behaviors:

- `UpArrow` searches server history backward by the current prefix.
- `DownArrow` moves forward through the server search results and then restores the typed prefix.
- `Ctrl+p` and `Ctrl+n` use the same server history search as `UpArrow` and `DownArrow`, which keeps Emacs key bindings working.
- `UpArrow`/`DownArrow`/`Ctrl+p`/`Ctrl+n` search only commands last run from the current resolved directory.
- `Shift+UpArrow` and `Shift+DownArrow` search global server history without filtering by directory.
- Inline prediction uses the current resolved directory when searching the history server, so the gray suggestion text can be accepted with `RightArrow`.
- `AddToHistoryHandler` sends accepted commands to the server.
- PSReadLine file saving is set to `SaveNothing` to avoid the shared-home history-file race.
- If the server is down or the token is missing, add failures are ignored and arrow search falls back to PSReadLine local history.

Optional timeout:

```sh
export PWSH_HISTORY_TIMEOUT_SEC=1
```

## Minimal systemd user service

Example:

```ini
[Unit]
Description=PowerShell history server

[Service]
ExecStart=/path/to/pwsh-history-server --db %h/.local/share/pwsh-history/history.sqlite3 --port 37373 --token change-this-long-random-token
Restart=on-failure

[Install]
WantedBy=default.target
```
