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
