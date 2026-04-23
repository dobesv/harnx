---
harnx: patch
---
Fix Ctrl-C not actually interrupting the agent (#292).

`SseHandler::text()` and `thought()` now check the abort signal before
forwarding chunks, so content arriving after Ctrl-C is silently dropped.
The Ctrl-C key handler no longer immediately resets the abort signal — the
reset is deferred to the next prompt submission, giving the background task
time to observe the cancellation.
