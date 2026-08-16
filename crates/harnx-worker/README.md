# harnx-worker

The worker daemon that executes harnx agent turns. Front-ends (`harnx`,
`harnx-serve`) don't run the agent loop themselves — they append the user
message to a NATS session log, and a worker leases the session and runs the
loop, including tool calls and hooks.

## Usage

```sh
harnx-worker --cluster local --worker-id worker-1
```

- `--cluster` — key from `nats_servers/<name>.yaml`, or `__local__` to use the
  shared local broker handed off via `HARNX_NATS_URL` / `HARNX_NATS_TOKEN`.
- `--worker-id` — stable identity for leases and the durable consumer name.
  Defaults to a generated id. Recommended in production so a restart rejoins
  its own consumer.
- `-x KEY VALUE` / `--agent-variable KEY VALUE` — agent variables, repeatable.
- `--diagnose` — start this configuration's tool servers, report which ones
  registered and how many tools each advertises, then exit without serving
  sessions.

Run several workers against one cluster for redundancy. Only one holds the
active lease for a given session; if it dies, another picks the session up.

## Local use

You normally don't launch this by hand. For the default `__local__` cluster,
`harnx` and `harnx-serve` start a worker themselves and supervise it for the
front-end's lifetime, so `harnx-worker` needs to be installed where they can
find it:

1. `HARNX_WORKER_BIN`, if set, must point at the binary.
2. Otherwise a `harnx-worker` next to the running front-end.
3. Otherwise `harnx-worker` on `PATH`.

The worker logs to stderr, and a front-end that started it points that at its
own log file — `~/.local/state/harnx/harnx.log` by default. So does everything
the worker starts in turn. Check there when a front-end reports that the worker
never became ready. Running the worker by hand, redirect it yourself:
`harnx-worker --cluster __local__ 2>> worker.log`.
