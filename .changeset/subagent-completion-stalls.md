---
harnx: patch
---
Recover the shared local NATS broker in the background while preserving its endpoint and surviving workers. Share bounded read/CAS recovery, lease-safe retries, cancellation watch recovery, and acknowledged tool/hook replies across the runtime. Report unconfirmed completion instead of waiting indefinitely, and restore missed sub-agent completion from durable results. Restart all local frontends and update worker/tool/hook binaries together to enable the recovery contract.
