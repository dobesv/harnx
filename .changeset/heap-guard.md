---
harnx: minor
---
Add a heap-usage guard to catch the intermittent runaway-allocation OOM (#842). The `harnx` and `harnx-acp-server` binaries install a global allocator that tracks live heap and, the instant it crosses a ceiling, captures a backtrace (whose top frames are the runaway allocation site), writes it to stderr and the log file, then aborts — before the process exhausts the machine and is OOM-killed with no stack. It is armed by default at 4096 MiB; set `HARNX_HEAP_LIMIT_MB` to change the ceiling, or `HARNX_HEAP_LIMIT_MB=0` to disable it. When disarmed it is a passthrough to the system allocator.
