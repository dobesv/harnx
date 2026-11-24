---
harnx: patch
---

Stop `history_snapshot` from returning assistant reasoning inline as a `<think>` block

Snapshot `content` now matches the answer text the live AG-UI stream sends. Reasoning comes back separately in a `history_reasoning` map, and SSE clients that send `x-harnx-snapshot-features: history-reasoning` also get a `history_reasoning` custom event after each snapshot. The web UI reads it to keep showing collapsed thinking after reloads and in archived history.

Upgrade note: web UI builds older than this release won't show thinking after a reload against a new server, and the web UI only sends the opt-in header to its own origin, so cross-origin deployments get answer-only snapshots in the main transcript.
