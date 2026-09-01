---
harnx: minor
---
Add per-invocation timeout and token budget controls (`--timeout-secs`, `--token-budget`) to CLI one-shot prompts and sub-agent tool calls (`{agent}_session_prompt`). When a limit is reached, invocations are hard-cancelled and return a synthesized explanation alongside machine-readable termination details, while leaving the session consistent for same-session retries. Interactive TUI and Web UI paths remain unbounded by design.
