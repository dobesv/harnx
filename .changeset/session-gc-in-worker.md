---
harnx: minor
---

Session garbage collection moved from the `harnx` CLI into the worker daemon, so headless `harnx-serve` + `harnx-worker` deployments now collect expired sessions; workers log a warning when retention (`cleanup_remote_sessions_days`) is unset.
