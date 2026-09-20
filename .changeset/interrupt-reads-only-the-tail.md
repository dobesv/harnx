---
harnx: patch
---
Interrupt requests and prompt appends read only the session log's last entry before their fenced append instead of the whole transcript, so their latency no longer grows with the session's length. The worker's interrupt hint check and its tail lookup for fenced appends read the same single entry.
