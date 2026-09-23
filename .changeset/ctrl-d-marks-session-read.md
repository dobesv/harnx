---
"harnx": patch
---

fix(tui): mark sessions read when exiting with Ctrl+D

Idle Ctrl+D now clears durable unread state even when the TUI's cached unread flag hasn't received the latest NATS invalidation yet.
