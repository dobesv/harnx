---
harnx: patch
---
Make `time_wait` and `time_wait_until` stop before the caller's run deadline with `stopped_by: "caller_deadline"`, leaving time to report back before the run is cancelled.
