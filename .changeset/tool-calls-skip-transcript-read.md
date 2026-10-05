---
harnx: patch
---
Stop workers reading the calling session's whole transcript before every NATS tool call. To journal a call, the worker looked up the sequence of the `ToolCalls` entry that made it by loading that transcript, one JetStream request per entry, so each call from a long session paid for a full read; on a bench replaying staging load those reads took 7.9% of worker CPU. The worker now hands down the sequence it got when it appended the entry, or, for a round resumed after approval or a worker restart, the one it read to resume it. Calls an operator runs directly, such as with `harnx call tool`, no longer read the transcript either.
