---
harnx: patch
---
Plans, bash, and grep tool servers now return filesystem and validation errors as recoverable tool results instead of fatal errors, so a failed tool call no longer halts the agent session.
