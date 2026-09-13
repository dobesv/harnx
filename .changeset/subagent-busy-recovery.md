---
harnx: patch
---
Recover completed sub-agent status and counters in attached TUI sessions while the parent continues working, including across compaction. Preserve queued live events and handoffs during recovery, and preserve live counters when start events are repeated.
