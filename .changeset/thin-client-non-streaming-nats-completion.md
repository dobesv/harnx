---
harnx: patch
---
fix: thin client now waits for assistant reply to current NATS turn instead of returning early on transient Idle state, and returns no stale prior response on abnormal turn termination
