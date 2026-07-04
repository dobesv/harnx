---
harnx: minor
---

Adds opt-in background GC for remote sessions stored in NATS KV. Enable via `cleanup_remote_sessions_days` config field or `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS` environment variable. When set, runs hourly to purge stale session index entries across all configured NATS clusters.
