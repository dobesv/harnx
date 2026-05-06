---
harnx: minor
---
Remove `*_default_session` configuration options.

The `tui_default_session`, `cmd_default_session`, and `agent_default_session`
configuration keys (and the legacy `repl_default_session` alias) have been
removed. These options auto-loaded a named session on startup but are no
longer needed.

The associated environment variables `HARNX_TUI_DEFAULT_SESSION`,
`HARNX_CMD_DEFAULT_SESSION`, `HARNX_AGENT_DEFAULT_SESSION`, and the legacy
`HARNX_REPL_DEFAULT_SESSION` are also removed. These environment variables
previously set default session names via `load_envs()`, but that logic no
longer applies env-based session defaults.

**Migration:** Use the `-s <session>` flag to explicitly specify a session
when starting harnx, or use `.session <name>` inside a running session.

**Important:** Existing config files and environment variables that contain
these keys/variables will be silently ignored. If you rely on environment
variables to set a default session, you must switch to using the `-s` flag
to avoid silent upgrade breakage.
