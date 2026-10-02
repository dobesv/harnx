---
harnx: patch
---
Fix the TUI staying busy forever after a session was compacted. Compaction writes the messages it keeps to the log again, and the TUI, the front ends' pending-prompt activation and the web follower read the newest of those copies as a prompt no turn had answered yet. They now skip messages compaction re-logged. The web UI also clears an automatic compaction's spinner when the run ends, in case its completion event was lost.
