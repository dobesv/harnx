# High-Availability Deployment (NATS Mode)

Harnx supports a distributed, high-availability (HA) mode backed by [NATS JetStream](https://nats.io/). In this mode, the agent tool loop runs in a dedicated **worker** process, while thin **clients** (TUI, CLI, or ACP server) connect to the worker via a NATS cluster.

## Architecture Overview

Harnx NATS mode decouples the execution of an agent from the user interface:

- **Clients**: Thin processes (TUI, ACP, CLI) that post user messages and render events. They do not execute tools.
- **NATS Cluster**: The central nervous system. Uses JetStream for a durable session log and Key-Value (KV) for leader election (leases).
- **Workers**: One or more daemon processes that execute the agent loop. Multiple workers provide redundancy; only one worker holds the active lease for a given session at a time.

## Prerequisites

- **NATS Server 2.11+**: Requires JetStream and Key-Value support.
- **Configuration**: A NATS cluster configuration file in your `nats_servers/` directory.

## Standing up NATS

For development, a single NATS server with JetStream is sufficient:

```bash
nats-server -js
```

For production HA, run a NATS cluster with at least 3 nodes and set `replicas: 3` for JetStream assets.

### JetStream Resources
Harnx automatically manages the following JetStream resources:
- **KV Bucket**: `harnx_leases` (Stores session leases with TTL).
- **Streams**: `SESSION_<id>` (Subject: `sessions.{id}.log`) stores the durable append-only session history.

## Configuration

Harnx looks for NATS cluster definitions in `nats_servers/<cluster_key>.yaml`. The filename (stem) is used as the cluster key.

### Example: Development (Plaintext)
`nats_servers/local.yaml`:
```yaml
url: "nats://localhost:4222"
```

### Example: Production (Token + TLS)
`nats_servers/prod.yaml`:
```yaml
url: "nats://nats.example.com:4222"
token: "${NATS_TOKEN}"
tls: true
tls_cert: "/etc/harnx/client-cert.pem"
tls_key: "/etc/harnx/client-key.pem"
# Note: tls_ca + client cert is NOT supported; use trusted certs or drop tls_ca.
```
*Note: Environment variable expansion `${ENV_VAR}` is supported in all fields.*

## Running Workers

A worker joins a cluster and waits for session assignments.

```bash
harnx worker --cluster local --worker-id worker-1
```

- `--cluster`: The key from `nats_servers/`.
- `--worker-id`: (Optional but recommended) A stable identity for the worker.

You can run multiple workers for redundancy. If the active worker for a session dies, another worker will acquire the lease and resume execution.

## Using Remote Agents

To use an agent via NATS, append `@cluster` to the agent name:

```bash
harnx -a coder@local
```

This works from the CLI, TUI, and ACP server.

- **New Sessions**: A new session log and lease are created in NATS.
- **Resuming Sessions**: Clients attach to an existing `session_id`. Multiple clients can attach to the same session simultaneously (Multiplayer Mode).

## Control Plane

Clients communicate with the active worker over specific NATS subjects:
- **Cancel**: `sessions.{id}.control` — Deliver a cancel command to the worker.
- **Set Pending**: Update the pending user message without triggering execution.

Semantics are consistent because the authoritative state is derived from the durable JetStream log.

## Multi-Client Support

Multiple clients can attach to a single session:
1.  Clients replay the **durable history** from the JetStream stream.
2.  Clients subscribe to **live advisory events** on `sessions.{id}.events` for real-time updates (streaming chunks, tool progress).

Late-joining clients automatically converge to the same state by replaying the durable log.

## Failover & Safety

### Leases & Fencing
Harnx uses a renewable CAS (Compare-And-Swap) lease in NATS KV:
- **TTL**: ~30 seconds.
- **Renewal**: Every ~10 seconds.
- **Fence Token**: The KV revision of the lease. Every write to the durable log is gated by this token.

If a worker loses its lease (e.g., network partition), it immediately aborts. This prevents "split-brain" scenarios where two workers think they are active.

### Resume & Idempotency
When a worker resumes an interrupted session:
- **Idempotent Tools**: Tools marked with `idempotent_hint` or `read_only_hint` in MCP are re-run if their result was lost.
- **Non-idempotent Tools**: If a result is missing for a non-idempotent tool, Harnx synthesizes an "interrupt-error" result to prevent accidental double-execution of side effects.

## Cleanup

Session logs and leases persist in JetStream until explicitly deleted.

```bash
harnx session delete <session_id> --cluster local
```

## Observability

### Logs
Workers emit structured logs for:
- Lease acquisition, renewal, and loss.
- Fenced-write rejections.
- Session activation and failover.

### Metrics
Harnx tracks internal counters (exported to logs and future metrics endpoints):
- `active_sessions_per_worker`: Current active loops.
- `lease_acquisitions` / `lease_losses`: Lease churn.
- `fenced_writes_rejected`: Safety triggers.
- `interrupt_errors_synthesized`: Data points on failover impact.

## TLS Support Note
Harnx supports TLS and mTLS for NATS connections. While token authentication and config-based TLS have been verified, automated PKI-backed integration tests for live TLS handshakes are ongoing.
