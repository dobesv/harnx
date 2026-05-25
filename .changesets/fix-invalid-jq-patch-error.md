---
harnx: patch
---
Invalid jq expressions in package patches now produce errors instead of being silently ignored. Agents, MCP servers, and clients whose patch expressions fail to compile or execute are skipped and logged as errors rather than loaded with their config unchanged.
