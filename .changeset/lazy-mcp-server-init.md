---
harnx: patch
---
fix(mcp): only initialize MCP servers whose tools match the agent's `use_tools` selectors, so unused servers no longer connect at startup or emit spurious "failed to connect" warnings (#790)
