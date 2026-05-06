---
harnx: patch
---
Fix `.edit session` corrupting the session when tool calls are present.

The full-save path (used by `.edit session` and the exit handler) now
correctly serializes tool-call rounds as `tool_calls`/`tool_results`
log entry pairs instead of the legacy `message` entries that the loader
rejects.
