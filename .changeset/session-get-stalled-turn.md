---
harnx: minor
---

Report a turn whose worker is gone as `stalled` in `session/get`, on every replica.

`session/get` used to report `running` forever for a durable prompt no worker ever claimed, or after the worker's lease disappeared, and a replica whose follow had just failed cached `idle` beside a stale `last_turn`. The state is now derived on each read from the log and the lease sample, the same way on every replica:

- `{ "status": "stalled", "reason": "unclaimed", "prompt_seq": N }` — the prompt at that sequence waited past `nats_lease_acquisition_timeout_secs` (default 60) without a worker lease.
- `{ "status": "stalled", "reason": "worker_lost", "prompt_seq": N }` — a worker wrote fenced output for that prompt's turn, no lease is held, and the follower watchdog's missing-lease grace has passed since the worker's last timestamped log write. An approval decision records no timestamp, so a turn whose newest worker row is the decision reports `running` until the worker's next timestamped write.

A lease present, a young prompt, or an approval gate still reports `running`/`awaiting_approval`, and a worker that claims the prompt later turns the state back to `running` on the next read — nothing is written to the log. `last_turn` keeps describing the last `turn_end`, `error` or `cancel`, so a stalled turn no longer masquerades as a finished one. `canPrompt` and `canCancel` stay true so clients can settle the stalled prompt by hand.
