---
harnx: patch
---
Thread explicit per-session working directories through harnx runtime so in-process runs keep tool execution, hooks, and persisted session metadata bound to each session's own working directory while preserving CLI and ACP fallback to process cwd when no per-session directory is set.
