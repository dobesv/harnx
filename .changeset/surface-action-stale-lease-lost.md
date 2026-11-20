---
harnx: patch
---

Surface action submissions no longer fail with a spurious `lease_lost` while the action proceeds. A stale worker (paused or reconnecting) that lost the session lease used to answer resends with `lease_lost` before acknowledging; that reply could land after the real lease holder's `Pending` acknowledgement and become the client's terminal outcome, reporting failure for an action that ran. A worker that cannot prove lease ownership now refuses without a wire reply, so only a worker that proved ownership when it acknowledged can report `lease_lost`. Clients still get a definite answer: from the holder, or — when no holder answers at all — from the lease-checked idle fallback after the recovery budget.
