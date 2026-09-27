---
harnx: patch
---

Project subagent progress into the shared tool-call update path for ACP and Web clients.

- ACP: `TurnEvent::SubAgentProgress` now emits `ToolCallUpdate` with title containing child session title and compact usage (e.g., "atlas — Analyzing code (100→50)").
- ACP: Usage structured under namespaced `_meta` (`harnx:usage`).
- ACP: Rich internal states (`Cancelling`, `Unconfirmed`) preserved as `InProgress`; `Cancelled` maps to `Failed`.
- Web: Subagent progress now emits both legacy `sub_agent_progress` and projected `tool_update` events for incremental client migration.

The `invocation_id` field correlates to the parent tool call. The `SubagentProgressReporter` behavior and its 10-second polling cadence remain unchanged.

Addresses #2096.
