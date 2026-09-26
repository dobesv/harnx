---
harnx: patch
---

Add `tool_update` SSE custom event for live tool progress. Emits `ToolEvent::Update` fields (title, status, kind, locations, usage, markdown) so web clients can apply in-place updates to tool call cards. Part of Phase 5b (#2096).
