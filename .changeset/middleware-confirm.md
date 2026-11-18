---
harnx: minor
---
Add Bash and Python middleware confirmation, accepted-answer lookup, and per-frame resume stashes. Confirmation requests stop the interpreter and return host-side checkpoints, including nested frames and saved result snapshots. Explicit step keys preserve interaction identity when reruns skip calls; restored envelopes keep their media and private metadata. Inner script calls fail closed on confirmation, and operator root calls auto-approve. Durable persistence and frontend response handling require the interaction driver.
