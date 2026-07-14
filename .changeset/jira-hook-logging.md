---
harnx: patch
---

fix(example): robust config parsing, lazy init, and diagnostics for jira-auth-hook.py

- **Config parsing**: use PyYAML when available, else an indentation-agnostic
  line parser. The old hand-rolled parser required list items indented exactly
  `  - ` and silently parsed **zero profiles** for other (valid) layouts,
  producing `profile matching current_profile not found` and no auth injection.
- **Lazy init**: read the acli config + keyring on the first Atlassian request
  instead of at startup, retrying on failure — so the hook never touches the
  keyring before it's ready and a transient miss isn't cached for the process's
  lifetime.
- **Diagnostics**: step-by-step logging (never the token), optional
  `HARNX_JIRA_LOG_FILE`, a full traceback on failure, and a `/jira-auth-hook/debug`
  endpoint reporting `initialized`, `target_hosts`, and the captured `error`.
- Fall back to `ATLASSIAN_EMAIL` when the profile has no email (was producing a
  blank Basic-auth username).
