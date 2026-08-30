---
harnx: patch
---
Clean up agent hook routes after each completed NATS-backed turn so stale fail-closed hooks cannot block later tool calls.
