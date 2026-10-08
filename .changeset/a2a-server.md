---
harnx: minor
---

feat(a2a-server): add harnx-a2a-server binary for A2A 1.0 JSON-RPC and SSE agent exports (#2315)

Add `harnx-a2a-server` to expose explicitly selected harnx agents over the Agent2Agent (A2A 1.0) protocol. Each agent receives a dedicated JSON-RPC endpoint and Agent Card under `/agents/{name}`.

Key features:
- **Server-allocated contexts**: Context IDs match durable harnx session IDs; task IDs use `{contextId}.{uuid}`.
- **Durable task lifecycle**: Supervised turn execution with unary (`SendMessage`, `GetTask`, `CancelTask`, `ListTasks`) and SSE streaming (`SendStreamingMessage`, `SubscribeToTask`) methods.
- **Message deduplication**: In-memory LRU cache for new sessions and session KV deduplication for subsequent turns by `messageId`.
- **Payload mapping**: Structured data parts rendered as fenced JSON blocks for the LLM; inline image raw files passed as multimodal media inputs; rendered data capped by `--max-data-part-bytes`.
- **Input compatibility**: Accepts A2A 0.3 method aliases, legacy `blocking`, and relaxed enum formats, emitting canonical A2A 1.0 responses.
- **User isolation**: Optional `--user-id-header` mode ensures sessions and tasks are restricted to their authenticated owners.

Task listing uses per-session metadata indexes and loads only requested page records (plus records needed for reconciliation or filters). Streaming keeps 100 ms deltas and fresh live snapshots for GetTask and reconnects, with intermediate artifact persistence at most every 2 seconds and full final persistence. Blocking sends wait on completion notifications instead of polling KV. After a server crash, interrupted tasks keep the last durable artifact snapshot, which can trail live output.

New contexts start with empty task indexes without scanning the shared bucket. Task indexes migrate legacy sessions on first access. Stop old servers before starting a new version against the same bucket. After a downgrade to a pre-index build, delete affected sessions’ `a2a/index` keys before re-upgrading so they are rebuilt. Index size is bounded by the bucket/broker limit; terminal tasks are retained.
