---
harnx: patch
---
Recover parked A2A tool approvals and accepted replies across backend restarts and replica changes. Recovery follows the original invocation, preserves full rejection notes and reply deduplication, and picks up decisions made by other frontends without admitting another prompt.
