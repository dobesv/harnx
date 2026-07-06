---
harnx: minor
---
Add AG-UI Phase 2 control plane to harnx-serve: a per-(agent,session) actor with a tokio::broadcast event bus, a JSON-RPC 2.0 control endpoint (`session/get|prompt|cancel`), and a subscription-style SSE endpoint that emits a MESSAGES_SNAPSHOT on join then streams live events to all subscribers (with ~15s keep-alive). Dropping an SSE connection no longer stops a run — only `session/cancel` aborts, and cancellation persists partial state. The SSE run POST now inspects only the last message and drops the previous reconcile/empty/multi-message 400s.
