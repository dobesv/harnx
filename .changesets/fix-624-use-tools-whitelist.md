---
harnx: patch
---
Fix `use_tools` whitelist being bypassed in `select_tools` — all MCP tools were sent to the LLM regardless of the whitelist, exceeding OpenAI's 128-tool limit for agents with many MCP servers. Now only the whitelisted tools are sent.
