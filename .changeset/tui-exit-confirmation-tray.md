---
harnx: minor
---
Make session cancellation durable and generation-scoped, propagate it through tools, hooks, and nested sub-agents, and keep new prompts blocked until shutdown is confirmed. Add direct child cancellation controls, static unconfirmed/retry states, and a responsive full-width TUI exit tray that exits on durable acceptance. Upgrade the internal tool protocol to v2; deploy frontends, workers, and tool servers together because v1 tool registrations are rejected.
