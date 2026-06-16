---
harnx: patch
---
Add diagnostic instrumentation for the intermittent out-of-memory crash (#842). The TUI event loop now runs a low-overhead memory watchdog that, once per second, logs (at `warn`) a snapshot of process RSS, transcript item count and text size, and the event-channel backlog whenever RSS crosses a doubling threshold — plus a warning when a single tick drains an abnormal number of events (a flooding producer). Compaction now logs when it starts and finishes (with duration) and flags a compaction triggered while another is still running. These surface in the harnx log file, so the next occurrence shows whether the growth is in the transcript/event path or elsewhere. Enable logging (set a non-`off` log level; `info` captures compaction detail) to collect it.
