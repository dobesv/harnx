---
harnx: patch
---

Record the lease holder's process identity (pid, a one-way hash of the host identity, and on Linux process start time) in session lease records, and let a worker on Linux reclaim the lease immediately when that holder is provably dead on the same host, instead of waiting out the lease TTL. This removes the roughly 30-second stall when re-running a session after a one-shot `harnx prompt` exited on `--timeout-secs` and killed its local worker. Any uncertainty — old records without the identity, a different host or pid namespace, a live pid, or a platform that can't record a strong host identity — keeps the previous TTL-only behavior.
