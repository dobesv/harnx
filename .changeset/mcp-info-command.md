---
harnx: minor
---

feat(commands): add `.info mcp [server]` diagnostics

Print an MCP server's resolved command, args, env, roots, connection status,
child PID, and — crucially — the exact `command` string of each configured
hook plus the **live PID of any running persistent hook** (e.g. the
`harnx-proxy-auth` process). Seeing the hook command verbatim and its PID makes
it easy to spot YAML-folding/argument-dropping problems or a hook that never
spawned. With no server name, lists all running servers with status and PID.
