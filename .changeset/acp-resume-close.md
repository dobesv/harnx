---
harnx: minor
---

Add `session/resume` and `session/close` support to ACP server.

`session/resume` validates session ownership and establishes in-memory context
without replaying history updates. Useful when clients already know session state
and just need to reestablish context for subsequent prompts.

`session/close` removes session context while preserving durable history. Active
turns are cancelled, but transcripts, metadata, and listing visibility remain
intact. Close is idempotent — unknown or already-closed sessions succeed.

Both capabilities are now advertised in `initialize` responses:
- `sessionCapabilities.resume`
- `sessionCapabilities.close`
