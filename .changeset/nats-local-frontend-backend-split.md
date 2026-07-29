---
harnx: minor
---

Make the shared-local-nats-server front-end/back-end split the only local execution path. TUI, one-shot CLI, serve, and ACP sessions now run every local turn front-end → NATS → worker; the old in-process local path is removed. This architectural change enables future work on tool servers and sub-agents over NATS (Phase 2). References issue #1224.
