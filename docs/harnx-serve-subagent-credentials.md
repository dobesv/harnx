# Troubleshooting Sub-Agent Credentials in `harnx-serve`

A common issue arises where ACP sub-agents (delegated agents) work perfectly in the CLI or TUI but fail with "missing credentials" or "API key not found" when running under the `harnx-serve` HTTP server.

This is almost always due to the **execution context** of the `harnx-serve` process differing from your interactive shell.

## The Root Cause

`harnx-serve` does not filter or clear the environment. It spawns sub-agents as child processes that inherit its own environment exactly as the TUI or CLI does. If a sub-agent fails to find credentials under `harnx-serve` while working elsewhere, the server process itself is likely missing the necessary environment variables or is resolving configuration files from a different path.

Common reasons include:
1. **Missing Shell Exports**: If you launched `harnx-serve` via a systemd unit or a background manager, it may not have inherited the `export CLAUDE_API_KEY=...` lines from your `.bashrc` or `.zshrc`.
2. **Path Resolution Failures**: `harnx` looks for its `.env` file in `HARNX_DATA_DIR` or `~/.local/share/harnx`. If `harnx-serve` runs as a different user or with a different `HOME`, it may look in the wrong place.
3. **Keyring Access**: Credentials stored in the OS keyring (via `secret-tool` or `gcp-auth-proxy`) require `DBUS_SESSION_BUS_ADDRESS` and `XDG_RUNTIME_DIR`. These are often missing in non-interactive service contexts.

## Diagnostics

`harnx-serve` provides a redacted environment snapshot at startup when the log level is set to `info`. This is the first place you should look.

Run the server with info logging enabled:
```bash
HARNX_LOG_LEVEL=info harnx-serve
```

Check the **Startup Environment Diagnostics** block in the logs first to identify discrepancies between your shell and the service environment. This block includes:
- Presence of critical variables: `HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, `XDG_STATE_HOME`, `DBUS_SESSION_BUS_ADDRESS`, `HARNX_DATA_DIR`, `HARNX_STATE_DIR`, `HARNX_ENV_FILE`, and `PATH`.
- The resolved path to the `.env` file and whether it was actually **FOUND**.
- The resolved data directory.
- The **NAMES** of any visible credential-like variables — those ending in `_API_KEY`, `_TOKEN`, `_SECRET`, `_ACCESS_KEY`, or `_KEY` (values are never printed).

### How to Read the Diagnostics

| Symptom in Log | Likely Cause | Fix |
| :--- | :--- | :--- |
| `.env` file: **NOT FOUND** | `harnx-serve` is looking in the wrong directory. | Set `HARNX_ENV_FILE` or `HARNX_DATA_DIR` explicitly. |
| credential-like env vars: (none visible) | Variables were not exported to the service environment. | Pass variables via your service manager (systemd/Docker). |
| `DBUS_SESSION_BUS_ADDRESS`: **MISSING** | Keyring/Secret Service lookups will fail. | Ensure the service inherits the user session bus. |

## Recommended Fixes

### 1. Launch with the Correct Environment
If using **systemd**, prefer a user service over a system service. Note that
`PassEnvironment=` forwards variables **from the service manager's own
environment** — it does *not* read your interactive shell. A freshly started
user manager usually has an empty environment, so you must either populate the
manager first or point the unit at a protected `EnvironmentFile`.

**Option A — protected `EnvironmentFile` (recommended):** keep secrets in a
`0600` file owned by your user and load it into the service:

```ini
# ~/.config/systemd/user/harnx-serve.service
[Service]
ExecStart=/usr/bin/harnx-serve
# File readable only by you (chmod 600); one KEY=value per line.
EnvironmentFile=%h/.config/harnx/harnx-serve.env
```

```bash
# ~/.config/harnx/harnx-serve.env  (chmod 600)
CLAUDE_API_KEY=sk-...
OPENAI_API_KEY=sk-...
```

**Option B — import your session environment before starting the service:**
seed the user manager with the needed variables, then start (and keep
`PassEnvironment=` in the unit to forward them):

```bash
systemctl --user import-environment CLAUDE_API_KEY OPENAI_API_KEY
systemctl --user start harnx-serve
```

```ini
# ~/.config/systemd/user/harnx-serve.service
[Service]
ExecStart=/usr/bin/harnx-serve
PassEnvironment=CLAUDE_API_KEY OPENAI_API_KEY
```

For **Docker** or system-wide services, explicitly pass the variables and paths:
```bash
docker run -e CLAUDE_API_KEY=$CLAUDE_API_KEY -e HARNX_DATA_DIR=/home/user/.local/share/harnx ...
```

### 2. Use the `.env` File
Instead of relying on ambient shell variables, place your credentials in the `harnx` `.env` file. Ensure `harnx-serve` can find it by setting `HARNX_ENV_FILE` if your service environment uses a different home directory.

The default location is: `~/.local/share/harnx/.env`

### 3. Restore Keyring Access
If you rely on `gcp-auth-proxy` or other keyring-based tools, ensure your service manager provides the session bus:

```bash
# Example for manual service launchers
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus
harnx-serve
```
