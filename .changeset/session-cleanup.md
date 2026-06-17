---
harnx: minor
---
Add automatic cleanup of inactive sessions (#847). A new opt-in config key `cleanup_inactive_sessions_days` automatically deletes inactive session transcripts and their attachments after a configurable number of days. Activity is based on filesystem mtime; unset or 0 disables cleanup. Runs once at startup and hourly thereafter in all modes (TUI, CLI, serve); best-effort and fault-tolerant.