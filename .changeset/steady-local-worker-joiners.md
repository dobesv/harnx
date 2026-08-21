---
harnx: minor
---
Give every frontend its own supervised local worker while preserving the shared local NATS broker, session history, events, and session leases. Local activations now target the owning frontend's worker, including nested sub-agents, and `harnx-worker` replaces `--cluster __local__` with the frontend-managed `--session-scope __local__` mode. Worker diagnostics now also require `--session-scope __local__` instead of a configured `--cluster`.
