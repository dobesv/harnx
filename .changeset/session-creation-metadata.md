---
harnx: minor
---

`harnx-serve`'s `POST /v1/agents/{agent}/sessions` now accepts an optional JSON body with the same shape as the metadata PATCH (`title`, `variables`, `overrides`). The supplied fields are stored in the session's canonical metadata when the reserved ID is first created, so the session is never briefly visible without its intended settings. A bodyless POST keeps reserving with the configured defaults, and a creation title is only marked manual when the body says `"manual": true`, which suppresses later generated titles.
