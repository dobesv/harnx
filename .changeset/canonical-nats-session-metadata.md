---
harnx: minor
---
Replace NATS transcript headers and denormalized session indexes with canonical KV session metadata and activity records, including redacted metadata HTTP APIs.

This is a hard protocol cut: pre-upgrade NATS sessions are not migrated and
must be cleared before upgrading all frontends and workers together.
