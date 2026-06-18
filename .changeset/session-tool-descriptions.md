---
harnx: patch
---

Clarify generated tool descriptions for session delegation and agent handoff so agents understand session context semantics (#837). The `session_prompt` ACP tool now states that continuing a prior conversation requires passing the `session_id` returned by an earlier prompt call, and that omitting it starts a new empty session with no prior context. The `handoff` tool description is corrected to reflect that the target agent always starts fresh — prior conversation history is intentionally cleared, and only the `prompt` argument carries context. Documentation-string changes only; no behavior changes.
