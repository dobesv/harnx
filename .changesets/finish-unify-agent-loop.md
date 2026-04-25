---
harnx: minor
---
Finish the unify-agent-loop work: tool dispatch is now async end-to-end, the ACP server forwards sub-agent tool calls and source-tagged chunks to the parent, and `> agent ▸ session` headings render correctly through nested ACP delegations.

Concrete changes:
- `eval_tool_calls` and `execute_tool_round` are now `async fn` (the previous sync + `block_on_async` path panicked on the ACP server's current-thread runtime, so sub-agent tool calls never reached `default_emit_tool_call`).
- `build_tool_eval_context` takes the active agent's `use_tools` whitelist explicitly so the ACP server (which holds the agent on `Input`, not `Config`) gets a non-empty `allowed_tool_names`.
- `run_agent_loop` emits a sourced `TurnEvent::Started` after `_session_handoff` so every front-end's sink renders the new agent's heading (planner→executor snapshot regression).
- `AcpChunkSink` forwards `MessageChunk`, `Final`, and `ToolEvent::Started` events through a structured `AcpForward` channel; `spawn_notify_text` / `spawn_notify_tool_call` attach `agent`/`session` meta so the parent's `AcpNotificationClient::resolve_notification_source` recovers sub-agent identity for nested chunks. The redundant nested `subscribe_chunks` path in the ACP server is removed — the global sink is the single fan-out point.
- `repro_249_top_level_delegation_markers` no longer asserts `count == 1` on the response phrase; that was an artifact of OLD-code behavior where the sub-agent's MCP tool errored out. The function-name invariant (one top-level delegation marker) is unchanged.
