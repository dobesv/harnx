---
harnx: patch
---
Restore automatic terminal and browser title updates during a session. Title and compaction completion events emitted from detached maintenance tasks now reach the owning session's event sink instead of being lost to the worker's process-global sink.
