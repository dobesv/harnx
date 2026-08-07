---
harnx: minor
---
The NATS worker is now a separate `harnx-worker` binary, and the `harnx worker`
subcommand is gone. Run `harnx-worker --cluster <key>` where you used to run
`harnx worker --cluster <key>`.

Front-ends spawn the worker for the local cluster, so `harnx-worker` must be
installed alongside `harnx` / `harnx-serve` — releases publish it as its own
archive, and it ships in the Docker image. Discovery checks
`HARNX_WORKER_BIN`, then a sibling of the running front-end, then `PATH`.
`HARNX_BIN` no longer plays a part in it.
