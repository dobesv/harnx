---
harnx: minor
---
Require agent and session for all activity (#451).

- **CLI**: Running `harnx <prompt>` without `--agent/-a` now errors with a clear message instead of proceeding anonymously.
- **CLI**: Non-interactive runs auto-resume a matching session (same agent, terminal, git branch, remote, and working directory) instead of always creating a new one.
- **TUI**: Starting `harnx` with no agents configured now exits with a helpful error message.
- **Switching**: Agent and session can now be changed directly without running `.exit agent` or `.exit session` first.
- **Removed commands**: `.exit session` and `.exit agent` are no longer available (use `.agent <name>` or `.session <name>` to switch directly).
- **Cleanup**: Legacy message-file saving without an active session has been removed.
