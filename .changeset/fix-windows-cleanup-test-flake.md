---
harnx: patch
---

Fix a Windows CI flake in the `harnx-mcp-plans` cleanup test. `cleanup_deletes_stale_plan_but_keeps_fresh_plan` relied on millisecond-scale sleeps finer than Windows filesystem timestamp resolution, causing the fresh plan to be deleted too. The test now sets the stale plan's mtime explicitly via `filetime` and uses a generous retention margin, making it deterministic across platforms.
