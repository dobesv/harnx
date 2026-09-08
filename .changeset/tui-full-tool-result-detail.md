---
harnx: patch
---
TUI transcript detail view now shows the full, untruncated tool result the agent sees. Previously pressing ENTER on a tool call only re-showed the collapsed user-facing summary; it now includes all content parts (including assistant-audience text) that were hidden or truncated in the inline row.