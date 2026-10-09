---
harnx: patch
---

Bound A2A task events with 128 MiB Limits/DiscardNew storage and checkpoint-covered cleanup that preserves publication predecessors. Coordinate session deletion with global message reservations, recovery discovery, task events and the scoped lease; refuse unsettled work instead of losing pending publication.

Validate NATS stream policy at startup, enforce serialized authority/input/answer budgets, and export optional recovery diagnostics with `--metrics-addr`. Add real broker capacity, GC and backend/worker permission tests. Existing event streams need the documented drain and policy migration before startup; don't mix writers or automatically replay interrupted work.
