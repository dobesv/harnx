---
harnx: patch
---
Deliver A2A updates and blocking results through any replica using a durable one-event outbox and independent JetStream task readers. Each update commits its snapshot and cursor before publication; reconnects start with an authoritative snapshot, then contiguous updates. Retention gaps, broker failures and lag interrupt the reader instead of silently dropping deltas. Interrupted work isn't replayed. Global event/registry retention and production capacity calibration remain separate work.
