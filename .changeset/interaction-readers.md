---
harnx: patch
---
Accept durable interaction records in session-log readers and preserve completed tool results when another call suspends or fails. Park interaction-owned rounds and uncommitted selected handoffs during worker and A2A recovery instead of replaying tools or cancelling pending invocations. Checkpoint-store read failures also park work unless the bucket or key is confirmed missing. Durable interaction entries (such as `ToolCallCompleted` and `InteractionRequested`) are written to session logs whenever a tool round runs with a non-empty `middleware:` chain, including Daedalus handoff turns when the updated pantheon package is installed.
