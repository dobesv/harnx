---
harnx: minor
---

feat(nats): add instance-scoped Core NATS tool invocation and the `harnx-time-server` pilot. NATS tools coexist with existing stdio tools during migration, and configured tool and sub-agent children now inherit local broker credentials by design. References #1224.
