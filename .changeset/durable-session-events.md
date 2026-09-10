---
harnx: patch
---

Make committed-handoff navigation, token usage, sub-agent progress, and
tool-approval state durable in the session log so every client recovers them by
replay. Previously these were advisory-only fan-out events, so a client that
wasn't attached when they fired (reconnect, a second client, a backend restart,
or a different harnx-serve process) never saw them.

- Handoff, usage, and sub-agent start are recorded as durable session-log
  entries and rehydrated on attach; clients dedupe live vs. hydrated events by
  marker id.
- Tool approvals are now a worker-owned durable protocol. The lease-holding
  worker is the single writer of the approval request and decision; any
  harnx-serve routes a decision to it, and lease+fence gives single-winner
  semantics (no double-apply under concurrent or duplicate submissions). Pending
  approvals survive reconnect and backend restart, and the web and TUI clients
  use the same worker-routed path. The web approval UI now reviews one tool call
  at a time.
- Loading pre-change sessions still works (additive schema).
