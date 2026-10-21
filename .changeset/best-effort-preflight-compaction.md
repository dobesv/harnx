---
harnx: patch
---
Keep tool-heavy turns running when best-effort preflight compaction fails. Report the compaction failure and try the next request with the original history, while still stopping for interruption or an expired deadline. Compaction after a provider context-window rejection remains required before retrying.
