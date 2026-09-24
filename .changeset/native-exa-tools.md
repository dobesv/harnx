---
"harnx": patch
"coding": patch
"pantheon": patch
---

feat: add native harnx-exa-tools server, replacing the npx exa-mcp-server dependency for web search (#1269)

Ports the two default-enabled tools of the external TypeScript `exa-mcp-server`
(`web_search_exa`, `web_fetch_exa`) to a native Rust toolset server, following the
`harnx-grep-tools` precedent. The `coding` and `pantheon` package `exa.yaml` configs
now run `harnx-exa-tools` directly instead of bridging to `npx exa-mcp-server`, so web
search no longer needs Node.js.
