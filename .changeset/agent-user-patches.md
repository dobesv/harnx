---
harnx: minor
---
Add `patches` front-matter field to agents. The worker runs these jaq filters at session activation with the session creator's caller principals bound as `$user` (`user_id`, `users`, `groups`, `roles`), producing the effective agent configuration for tool declarations, script mode, and server demand. Caller principals are snapshotted at session creation and inherited by subagents and handoffs.
