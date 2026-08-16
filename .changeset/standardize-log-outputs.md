---
harnx: minor
---
Standardize logging across every binary. `HARNX_LOG_LEVEL` now configures all of
them (default `info`) and is inherited by subprocesses; `HARNX_LOG_FORMAT=json`
switches every process to one JSON object per line. The `harnx` CLI and TUI log
to `<state dir>/harnx.log` (was `harnx_runtime.log`, a name bug), overridable
with `HARNX_LOG_PATH`. Servers and subprocesses always log to stderr, and a
parent that logs to a file redirects their output there — so the worker and its
tool and hook servers land in the front-end's log instead of a separate
`harnx_worker.log`. `harnx-pkg`, `harnx-proxy-auth`, `harnx-sandbox-run`,
`harnx-mcp-remote`, and `harnx-mcp-time` previously ignored `HARNX_LOG_LEVEL`
entirely; `nats-server` output was discarded.
