---
harnx: patch
---

Show direct-child pending approval messages in terminal progress. In the TUI, press `a` on a child row or its detail view to answer legacy approvals or confirmation interactions using the child's exact request identity. Child confirmations wait behind foreground modals, report stale or conflicting answers, and leave the parent running. Non-interactive CLI progress displays awaiting state and messages without offering answers.
