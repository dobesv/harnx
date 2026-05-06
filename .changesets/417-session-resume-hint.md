---
harnx: minor
---
Print session resume instructions to the terminal on exit.

When harnx exits a saved session (TUI or CMD mode) it now prints:

```
Resume this session by running:
  harnx -a <agent> -s <session>
```

The `-a <agent>` flag is omitted when no agent is active. The hint is
suppressed for empty sessions and for sessions that are explicitly opted
out of saving (`save_session: false`).
