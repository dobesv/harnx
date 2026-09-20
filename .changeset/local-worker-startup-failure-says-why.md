---
harnx: patch
---

Say why the local worker died when it never becomes ready.

`LocalWorkerSupervisor` gave up after three worker exits with `local worker
exited 3 times without becoming ready:` and nothing after the colon. The exit
status was available and only logged, and the message tailed a log file that a
process without a configured logger never opens — so every test binary, and
every embedder that logs nothing, reported a startup failure with no evidence in
it.

The message now names each exit status and quotes what the worker printed. When
the parent has no log file to share, the supervisor captures the worker's output
into a temporary file of its own rather than discarding it.
