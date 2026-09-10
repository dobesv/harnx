---
harnx: patch
---
Stop logging a spurious "declining to lower" replica-count notice when a NATS KV bucket already has exactly the requested number of replicas. Only a request that would genuinely lower the count is logged now; an equal request is a silent no-op.
