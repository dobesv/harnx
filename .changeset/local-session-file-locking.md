---
harnx: minor
---

feat(session): cross-process file locking for local filesystem sessions

Multiple HarnX processes (TUI, Web UI, CLI, ACP) sharing the same local session
file are now serialized via a per-session `.yaml.lock` file (`std::fs::File::lock`).
A second process shows "Waiting for session lock…" in the transcript, then acquires
the lock when the first goes idle, reloads the session from disk to pick up entries
written by the prior holder, and proceeds. Session file writes (`save`, `append_event`,
`ensure_log_file`) no longer truncate or drop entries under concurrent access, and
sequence numbers are re-derived from the file while the lock is held to avoid stale
caches.
