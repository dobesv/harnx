---
harnx: patch
---

Add the internal plumbing that script mode will use to run tool calls from inside a `bash` script: a lossless single-call evaluator in the engine, a forked tool context that defers approvals and keeps inner calls out of the transcript, and a runtime executor that enforces `bash_tools`, caps concurrency and call counts, and rolls inner-call progress into the outer `bash` row. Payload limits are checked again after PreToolUse argument mutations, before provider dispatch. Post-hook notices from detached hook tasks now reach the session that triggered them.
