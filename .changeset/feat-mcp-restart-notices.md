---
harnx: minor
---

Surface MCP server transport failures and subprocess churn as user-visible notices, including reconnect warnings and child stderr/closure signals, so ACP/TUI users can see MCP restarts and deaths without digging through logs. Fixes #990.
