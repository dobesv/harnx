---
harnx: minor
---
Add long-lived tool service mode to `harnx-proxy-auth`, exposing the `proxy_auth` toolset with the `proxy_auth_environment` tool. `harnx-sandbox-run` queries the environment tool over MCP stdio and keeps the helper process alive while the sandboxed command runs.
