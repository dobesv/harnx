---
harnx: patch
---

Fix ACP prompt-time MCP/ACP manager churn so re-selecting same agent scope preserves running subprocesses instead of restarting MCP servers every turn. Also stop ACP single-threaded tool discovery from invalidating freshly connected MCP services. Fixes #988.
