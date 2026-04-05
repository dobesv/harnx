---
harnx: minor
---
ACP sub-agent tool calls, thoughts, plan updates, and token usage stats are now visible in the REPL with a spinner, instead of appearing as a silent pause. Token usage from sub-agents is propagated via `SessionInfoUpdate._meta` using the `harnx:usage` key. This works recursively for sub-sub-agent calls.
