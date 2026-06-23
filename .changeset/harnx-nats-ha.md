---
harnx: minor
---

High-availability distributed agent execution backed by NATS JetStream.

This feature enables a distributed mode where thin clients (TUI, CLI, ACP) communicate with backend workers over NATS. Key capabilities include:
- **Durable Persistence**: Session logs are stored as append-only streams in NATS JetStream.
- **High Availability**: Multiple workers can provide failover, using a NATS KV-based lease for single-active-worker mutual exclusion and fence tokens to prevent stale writes.
- **Thin Client Driver**: Automatic routing for `agent@cluster` agent references, separating client-side UI/tooling from backend execution.
- **Live Event Fan-out**: Real-time streaming of model chunks and status updates to multiple connected clients for multiplayer visibility.
- **Control Plane**: Remote cancellation and pending message management across the NATS cluster.
- **Security**: Support for NATS token authentication and mTLS.
- **Operations**: New `harnx worker` command, session management tools, and comprehensive HA documentation.
