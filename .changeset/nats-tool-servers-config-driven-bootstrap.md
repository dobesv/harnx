---
harnx: minor
---

feat(nats): declare NATS tool servers in `tool_servers/*.yaml` across user and package configuration directories instead of using a hardcoded server list. Tool servers are lazy-spawned based on the active agent's `use_tools` patterns, and a missing or crashing server emits a UI warning while worker execution continues. The `time` tool now ships as `harnx-time-server` under `tool_servers/`. References #1224.
