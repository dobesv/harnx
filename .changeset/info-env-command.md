---
harnx: minor
---

feat(commands): add `.info env` to inspect the harnx process environment

`.info env` lists the environment variable **names** harnx (and therefore its
hooks and MCP servers) inherit — values hidden. `.info env <NAME>` prints a
single variable's value. Useful for diagnosing hook/proxy problems (e.g. is
`DBUS_SESSION_BUS_ADDRESS` present, is a token var set) without dumping secrets.

Also adds `example_config/probe-auth-hook.py`: a standalone script that drives a
`harnx-proxy-auth` exec hook (e.g. `jira-auth-hook.py`) directly, showing its
debug/init state and the masked Authorization header it would inject per host.
