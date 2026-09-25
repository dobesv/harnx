---
"harnx-acp-server": patch
"harnx-engine": patch
"harnx-runtime": patch
---

Wire in-process tool progress updates for live status reporting:

- Add `emit_tool_update_fn` callback to `ToolEvalContext` for progress events
- Implement `RuntimeToolProgress` with 250ms coalescing and terminal ordering
- Call `provider.call_tool_with_progress` instead of `call_tool_with_id`
- Generate stable UUID tool-call IDs if the model doesn't supply one
- Update ACP mapper to preserve `None` status and map `title`/`kind`/`locations`
- Store per-call usage in namespaced `_meta.harnx:usage`

Implements Phase 2 of the tool live updates design (#2096).
Non-emitting tools continue to work unchanged via default implementation.
