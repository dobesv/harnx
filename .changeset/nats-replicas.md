---
harnx: minor
---

NATS cluster config accepts `replicas` to set the JetStream replica count for buckets harnx creates. Buckets created before `replicas` was set (or before it was raised) now get their live replica count reconciled up to match, alongside the existing TTL reconcile; a reconcile that a cluster can't satisfy is logged and skipped rather than stopping harnx from starting.
