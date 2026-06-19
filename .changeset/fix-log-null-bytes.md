---
harnx: patch
---
Fix the log file filling with NUL bytes (#880). harnx and the `harnx-acp-server` sub-agents it spawns share one `HARNX_LOG_PATH`; opening it with `File::create` gave each process an independent, truncating file offset, so concurrent writers clobbered each other and the kernel zero-filled the gaps. The log is now opened in append mode (`O_APPEND`), so every write lands atomically at end-of-file and concurrent processes interleave cleanly at line granularity. The file is no longer truncated per run. Each process also logs a `harnx start: v… build=<git sha> pid=… level=… log=…` line at startup so a shared log can be attributed per PID and the running build is verifiable.
