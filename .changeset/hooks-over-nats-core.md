---
harnx: minor
---

feat(nats): introduce core hooks-over-NATS protocol and worker dual dispatch.

Adds the `harnx-hookset` protocol crate and `harnx-hookset-server` daemon for NATS hook registration and request/reply execution. Worker dual dispatch runs NATS hooks alongside existing inline hooks, while hook supervision and config migration are deferred to future slices. References #1224.
