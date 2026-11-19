---
harnx: patch
---

Fix sessions staying parked after clearing or resetting a suspended middleware round. New prompts no longer recover cleared tool calls or handoffs, and old confirmation replies cannot authorize them. Private checkpoints remain inert until session deletion; clearing doesn't undo side effects that already happened.
