---
harnx: minor
---
Improve MCP tool result display formatting:

- Fix line-width truncation overflow that caused tool result lines to exceed terminal width by up to 5 characters.
- Parse MCP CallToolResult content parts and filter by `annotations.audience` — hide assistant-only content from the user display, show clean text instead of raw JSON.
- All 4 built-in MCP servers (bash, fs, time, todo) now return dual-audience content blocks: a concise user-facing summary alongside the full detailed result for the LLM.
