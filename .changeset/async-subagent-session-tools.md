---
harnx: minor
---

Add durable `session_send` and prompt-specific `session_wait` tools for asynchronous sub-agent work. Extend `session_load` with bounded paging, filters, and continuation metadata.

`session_load` now returns a bounded page by default instead of the legacy unbounded event array. Consumers that need more history should follow `next_index` or use `read.entry_index` with `read.byte_offset` to retrieve omitted content. The response retains `session_id` and `events`; event payloads use the shared rendered, source-indexed format.
