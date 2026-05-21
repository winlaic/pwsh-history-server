# pwsh-history-server

Centralized PowerShell history storage for Linux `pwsh` sessions that share a home directory.

The server is a small Rust HTTP service backed by SQLite. Each PowerShell process records and searches history through the server instead of racing on the same PSReadLine history file.

## Server

Set the three required environment variables:

```sh
export PWSH_HISTORY_DB="$HOME/.local/share/pwsh-history/history.sqlite3"
export PWSH_HISTORY_TOKEN="change-this-long-random-token"
export PWSH_HISTORY_BIND="0.0.0.0:37373"
```

Build and run:

```sh
cargo build --release
cargo run --release
```

The server creates the SQLite parent directory automatically and enables WAL mode.

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
  -d '{"command":"git status"}'
```

Search by prefix:

```sh
curl "http://127.0.0.1:37373/v1/history/search?prefix=git&limit=100" \
  -H "X-Pwsh-History-Token: $PWSH_HISTORY_TOKEN"
```

Response:

```json
{
  "entries": [
    {
      "id": 1,
      "command": "git status",
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
Environment=PWSH_HISTORY_DB=%h/.local/share/pwsh-history/history.sqlite3
Environment=PWSH_HISTORY_TOKEN=change-this-long-random-token
Environment=PWSH_HISTORY_BIND=0.0.0.0:37373
ExecStart=/path/to/pwsh-history-server
Restart=on-failure

[Install]
WantedBy=default.target
```
