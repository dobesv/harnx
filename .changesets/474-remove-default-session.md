---
harnx: minor
---
Remove `*_default_session` configuration options.

The `tui_default_session`, `cmd_default_session`, and `agent_default_session`
configuration keys (and the legacy `repl_default_session` alias) have been
removed. These options auto-loaded a named session on startup but are no
longer needed.

**Migration:** Use the `-s <session>` flag to explicitly specify a session
when starting harnx, or use `.session <name>` inside a running session.

Existing config files that contain these keys will simply have them ignored.
