---
harnx: patch
---
Recover pending tool invocations automatically after a worker restart. Tool servers can return saved replies, retry idempotent operations, or reattach sub-agent turns without duplicating their prompts. Fix a lease-release race that could delay restart recovery and false cancellation failures when completed execution records are pruned concurrently. This upgrades the internal tool protocol; restart all frontend, worker, and tool-server instances together.

Preserve completed child results and tool-call order during replay, and verify the original logical tool-server identity and current worker ownership before redispatch.
