---
harnx: minor
---

`harnx-worker` takes `--manage-servers` to launch its own tool and hook servers. Without it, the worker discovers independently deployed servers under `HARNX_SERVER_SCOPE`.
