# pwsh-history-server

Centralized PowerShell history storage for Linux `pwsh` sessions that share a home directory.

The server is a small Rust HTTP service backed by SQLite. Each PowerShell process records and searches history through the server instead of racing on the same PSReadLine history file.

## Quick start

Build and run:

```sh
cargo build --release
./target/release/pwsh-history-server
```

Without environment variables, the server uses:

```text
PWSH_HISTORY_DB=$HOME/.local/share/pwsh-history/history.sqlite3
PWSH_HISTORY_TOKEN=<random 32-byte hex token printed at startup>
PWSH_HISTORY_BIND=0.0.0.0:37373
PWSH_HISTORY_URL=http://<default-route-ip>:37373
```

The server creates the SQLite parent directory automatically and enables WAL mode.

## Configuration

Environment variables override the defaults:

```sh
export PWSH_HISTORY_DB="$HOME/.local/share/pwsh-history/history.sqlite3"
export PWSH_HISTORY_TOKEN="change-this-long-random-token"
export PWSH_HISTORY_BIND="0.0.0.0:37373"
export PWSH_HISTORY_URL="http://history-server-host:37373"
```

`PWSH_HISTORY_URL` is used by the PowerShell client. If it is not set and the bind address is `0.0.0.0` or `[::]`, the server detects the default-route IP and uses that address when `--lazy` writes the profile.

## Lazy PowerShell setup

Run the server with `--lazy` on a machine where `pwsh` is installed:

```sh
./target/release/pwsh-history-server --lazy
```

This copies `pwsh-history.ps1` to:

```text
$HOME/.config/powershell/pwsh-history.ps1
```

Then it updates `$PROFILE.CurrentUserAllHosts` with a managed block containing the effective client settings before sourcing `pwsh-history.ps1`. The block writes `PWSH_HISTORY_DB`, `PWSH_HISTORY_TOKEN`, and `PWSH_HISTORY_URL`; it does not write server-only settings such as `PWSH_HISTORY_BIND`. Paths under the home directory are written through `$HOME` instead of hard-coded absolute home paths. If the managed block already exists, it is replaced.

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
