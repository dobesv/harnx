---
harnx: major
---
**Breaking:** `HARNX_INSTANCE_ID` is renamed to `HARNX_SERVER_SCOPE`; the old name is no longer read at all. If you set `HARNX_INSTANCE_ID` by hand anywhere (a deployment manifest, a wrapper script, an independently deployed tool/hook server pod), rename it to `HARNX_SERVER_SCOPE` or that process silently falls back to minting its own scope instead of using yours. It is set automatically in normal use; set it explicitly only when deploying tool or hook servers independently of a worker.
