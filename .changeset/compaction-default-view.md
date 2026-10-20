---
harnx: patch
---
Show the latest compaction summary before kept and later messages across session history snapshots, the web interface, the TUI, the CLI session dump, ACP session loading, and TUI dump overlays. Add compaction metadata to session/get and live snapshots, and summary/range fields to compaction completion events. Keep archived messages in the TUI marker detail view without repeating them inline. Strip terminal control characters from replayed compaction notices in CLI dump.
