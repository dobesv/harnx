---
harnx: minor
---

Add an optional `agents:` list to NATS cluster config (`nats_servers/<cluster>.yaml`). Declared remote agents appear as `name@cluster` in `--list-agents`/shell completion, and assistant-role entries also appear in the interactive picker. Static config only — no network calls.
