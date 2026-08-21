# harnx-worker

The worker daemon executes Harnx agent turns. Frontends append user messages
to a NATS session log and publish activations; a worker acquires the session
lease and runs the model, tool, and hook loop.

## Persistent clusters

Run a cloud or otherwise persistent worker against a configured cluster:

```sh
harnx-worker --cluster prod --worker-id worker-1
```

- `--cluster` selects `nats_servers/<name>.yaml`. The reserved name
  `__local__` is rejected because local workers are owned and addressed by a
  frontend, not shared through cluster dispatch.
- `--worker-id` is the deployment identity used for leases and the durable
  cluster consumer. It defaults to a generated ID; a stable value is
  recommended in production.
- `--manage-servers` launches this worker's tool and hook servers. Without it,
  the worker discovers independently deployed servers under
  `HARNX_SERVER_SCOPE`.
- `-x KEY VALUE` / `--agent-variable KEY VALUE` sets an agent variable and may
  be repeated.

Multiple persistent workers compete through the same cluster-wide activation
stream. This cloud topology is unchanged.

## Frontend-managed local workers

`harnx` and `harnx-serve` each supervise one worker for their own process
lifetime. They use the `--session-scope __local__` execution mode with an
explicit generated worker ID, `--manage-servers`, and a broker credential
handoff in `HARNX_NATS_URL` and `HARNX_NATS_TOKEN`. This is an internal
frontend topology, not a manually operated local daemon mode.

Local frontends still share the broker, durable session logs, advisory events,
session listing, and session leases. Only execution is frontend-affine: an
idle prompt targets the submitting frontend's child. If another child already
holds the session lease, that holder consumes newly appended messages from the
shared log. Nested sub-agent turns target the same child as their parent.

Each child inherits its frontend's environment and current directory. Restart
the frontend to pick up intentional configuration, environment, working
directory, installation, or binary changes; a normal worker health check does
not hot-reload them. A crashed child is respawned on the same activation route.

Frontend and worker binaries may be compiled separately. Readiness accepts a
different build SHA when their readiness protocol is compatible. After an
upgrade that changes the local protocol, restart all local frontend and worker
processes; durable session history is preserved.

The frontend finds `harnx-worker` in this order:

1. The path in `HARNX_WORKER_BIN`.
2. A sibling of the running frontend binary.
3. `harnx-worker` on `PATH`.

Worker, tool-server, and hook-server output goes to the frontend's log file,
`~/.local/state/harnx/harnx.log` by default.

Local tool-server diagnostics remain available with:

```sh
harnx-worker --session-scope __local__ --diagnose
```
