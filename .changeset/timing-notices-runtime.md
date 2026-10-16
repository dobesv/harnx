---
harnx: minor
---

Deliver persisted current-run timing notices at safe pre-model boundaries in agent and sub-agent runs when timing milestones are due. Initial notice includes admission timestamp, elapsed time, and deadline if set. Deadline warnings at 50%/75%/90% budget thresholds, elapsed updates every 15 minutes. Notices are informative context, not executable prompts, and persist durably across replays and compaction.

Deployment requires a coordinated hard cutover: stop all old runtime readers before new always-on receipt writers start. Rollback must retain receipt-compatible readers; draining work does not make saved histories safe for pre-receipt binaries. Follow “Timing receipt compatibility cutover and rollback” in `docs/kubernetes-deployment.md`.
