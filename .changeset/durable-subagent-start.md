---
harnx: minor
---
Sub-agent delegations now record a durable start entry in the parent session log
carrying the child session_id, so a parent agent can resume or inspect a sub-agent
session even when the delegation is interrupted before returning. This adds a new
`sub_agent_started` transcript entry; in multi-instance clusters, deploy readers
that understand it before workers that write it.
