---
harnx: patch
---
Fix an infinite retry loop in the TUI when a queued message failed to send. Errored messages are now restored as editable drafts instead of being automatically replayed.
