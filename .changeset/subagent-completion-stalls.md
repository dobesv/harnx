---
harnx: patch
---
Recover the shared local NATS broker in the background while preserving its endpoint and surviving workers. Share bounded read/CAS recovery, lease-safe retries and acknowledged tool/hook replies across the runtime. Report a sub-agent whose completion cannot be established instead of waiting on it indefinitely, and restore missed sub-agent completion from durable results. Restart all local frontends and update worker/tool/hook binaries together to enable the recovery contract.
