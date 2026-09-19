---
harnx: minor
---

Support remote `agent@cluster` agents in `harnx-serve` (the HTTP API and Web UI), matching the CLI and TUI.

Agents declared in `nats_servers/<cluster>.yaml` now appear in `GET /v1/agents` (respecting the `role: assistant` picker filter) and can be addressed over HTTP as `/v1/agents/sisyphus%40shared`. Sessions against a remote agent are created, prompted, streamed, cancelled and resumed through the server, with turns running on a worker in the target cluster. A server that only talks to a remote cluster no longer starts a local broker or worker. A declared but unreachable cluster now returns a clear transport error naming the cluster instead of a misleading 404, and remote agents no longer vanish from the listing when their config can't be loaded as a local file. The internal `::__status=` marker no longer leaks into user-facing error messages.
