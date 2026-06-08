---
harnx: patch
---
Show session compaction in the transcript instead of a stdout spinner. Previously, triggering compaction (the `.compact session` command or automatic compaction) drew a spinner directly to stdout, which corrupted the TUI input area and left an uncleared line. Compaction now emits `CompactingStarted` / `CompactingCompleted` / `CompactingFailed` session events that the TUI renders as transcript entries and the CLI renders via a managed spinner. Manual compaction also guards against running concurrently with automatic compaction.
