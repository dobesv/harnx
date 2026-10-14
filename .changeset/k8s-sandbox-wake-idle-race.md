---
harnx: patch
---

Fix an issue where waking sandboxes could be suspended by the idle watcher before becoming ready (#2405). The gateway now records activity before waking a sandbox and sends keep-alive activity updates every 60 seconds while waiting for readiness. If an idle scan suspends a sandbox during activation, the gateway re-wakes it up to two recovery times (three wake attempts total) or fails fast with an explanatory error instead of hanging for 30 minutes.
