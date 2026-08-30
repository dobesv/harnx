---
harnx: patch
---
Let long-running sub-agent calls wait for lease-backed completion without implicit idle or one-hour deadlines, while detecting unavailable NATS tool servers through their registrations.
