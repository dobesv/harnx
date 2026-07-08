---
harnx: minor
---

harnx-serve and the AG-UI web client now support composing and injecting pending messages while an agent run is active. The server queues prompts sent via `session/prompt` (returning `Enqueued`) and consumes them on the next tool round, emitting a `pending_message_consumed` `CUSTOM_EVENT`. The web client uses this event to clear its queued message UI indicator reliably.
