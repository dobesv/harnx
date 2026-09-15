---
harnx: patch
---
Return successful tool-approval decisions when a worker's acknowledgement is lost but the decision is already durable. Repeating the same approval or denial is idempotent and does not execute the tool twice.
