---
harnx: minor
---

feat(hooks): structured notice channel + failure surfacing to the UI

Hooks can now surface messages to the active UI (TUI/CLI/serve) two ways:

- **Structured channel (live hooks):** a persistent hook prints a standalone
  JSONL line `{"notice": {"level": "error"|"warning"|"info", "message": "…"}}`
  on stdout (no request `id`). harnx recognizes it and posts an
  `AgentEvent::Notice`. `harnx-proxy-auth` forwards such lines from its exec
  sub-hooks, so a nested hook (e.g. `jira-auth-hook.py`) can report an internal
  error even while it keeps running.
- **Dead-child fallback:** when a persistent hook process fails to launch or
  exits unexpectedly, harnx emits an Error notice with the child's captured
  stderr tail (deduped per command within 30s).

`jira-auth-hook.py` uses the structured channel to report auth-init failures
(e.g. keyring/config problems) instead of failing silently.
