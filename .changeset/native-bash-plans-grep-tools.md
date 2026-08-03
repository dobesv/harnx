---
harnx: minor
---

feat(nats): convert `bash`, `plans`, and `grep` to native toolset servers and rename their crates/binaries (`harnx-mcp-bash` → `harnx-bash-tools`, `harnx-mcp-plans` → `harnx-plans-tools`, `harnx-vercel-grep-server` → `harnx-grep-tools`).

These three tool servers now implement the `Toolset` trait and run directly, dropping the `harnx-mcp-bridge` wrapper process. `--mcp-stdio` mode is retained on all three for backward compatibility. `harnx-mcp-bridge` stays as the adapter for external stdio MCP servers (fetch/exa/context7). The bash sandbox, git-snapshot history, proxy-auth hook, and the plans retention loop are unchanged. References #1224.
