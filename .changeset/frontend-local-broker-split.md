---
harnx: minor
---
Stop the front-end and worker broker split for local sessions by making the deployment model unambiguous. When `HARNX_NATS_SERVER` is unset, front-ends (`harnx`, `harnx-serve`) always self-host a local broker and worker for `__local__` sessions and ignore operator `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` for local session routing. This fixes issue #2021, where front-ends given external NATS credentials connected their session side to the external cluster while the spawned worker ran against an elected pod-local broker, causing sessions to silently hang.

If you set `HARNX_NATS_URL`/`HARNX_NATS_TOKEN` on `harnx-serve` (or the CLI/TUI) to reach an external NATS cluster, that no longer joins the cluster — the front-end self-hosts a local broker. Set `HARNX_NATS_SERVER=<name>` and add `nats_servers/<name>.yaml` (which can use `${HARNX_NATS_URL}`/`${HARNX_NATS_TOKEN}`).
