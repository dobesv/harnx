---
harnx: patch
---

Fix ACP/MCP subprocess log forwarding so explicit `HARNX_LOG_LEVEL` and `HARNX_LOG_PATH` propagate into child servers, preserve inherited log settings over `.env`, and support `{pid}` log-path templates for per-process files. Fixes #989.
