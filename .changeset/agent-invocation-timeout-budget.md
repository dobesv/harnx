---
harnx: minor
---
Add per-invocation timeout and token budget controls (`--timeout-secs`, `--token-budget`) to CLI one-shot prompts and sub-agent tool calls (`{agent}_session_prompt`). The two limits end the turn differently: a timeout interrupts it with a durable `Cancel`, while an exhausted token budget is caught worker-side at a round boundary and ends the turn with an error. Either way the invocation returns a synthesized explanation alongside machine-readable termination details, leaving the session consistent for same-session retries. Interactive TUI and Web UI paths remain unbounded by design.
