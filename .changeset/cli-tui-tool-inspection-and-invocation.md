---
harnx: minor
---

Add CLI, TUI, and stdio bridge commands to inspect tools and execute tools directly (#2251, #1045):

- CLI `harnx list tools [<pattern>]`, `harnx info tool <name>`, and `harnx call tool <name> <args-json>`, with `--json` output format and agent targeting via `-a` / `--agent`.
- TUI `.list tools [pattern]`, `.info tool <name>`, and `.call tool <name> <args-json>` dot-commands with autocompletion and background cancellation.
- Stdio bridge `harnx-mcp-bridge --call-tool <name> [--tool-args <json>]` standalone direct invocation with JSON output, pre-spawn argument validation, filter enforcement, and child process cleanup.
- Tool reservations for no-agent CLI commands, selecting tools without a default agent.
- Scoped operator direct invocation consent, auto-approving root confirmation requests while enforcing hook denials, argument mutations, schema validation, and isolated nested approvals.
