---
harnx: minor
---

NATS cluster config accepts `replicas` to set the JetStream replica count for buckets harnx creates: the tool/hook registries, session leases (`harnx_leases`), and the session index (`harnx_sessions`). Buckets created before `replicas` was set (or before it was raised) now get their live replica count reconciled up to match, alongside the existing TTL reconcile; a reconcile that a cluster can't satisfy is logged and skipped rather than stopping harnx from starting. A brand-new bucket requested with a replica count the cluster can't provide still fails to create, by design — this only changed the fix-in-place path for buckets that already exist.
