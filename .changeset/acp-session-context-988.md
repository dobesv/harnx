---
harnx: minor
---

Fix #988: ACP server now creates per-session context once instead of forking config per-prompt, preventing MCP subprocess respawn on every turn.

**Problem:** The ACP server re-derived config per prompt via `fork_prompt_config` → `use_agent_by_name` → `reinit_managers_for_agent`, which unconditionally rebuilt McpManager. This killed and respawned all MCP subprocesses (bash, fs, time, plans) on every prompt, breaking `read_exec_log` and degrading performance.

**Solution:** Introduced `SessionContext` that holds a forked `GlobalConfig` (with its own `McpManager`/`AcpManager`) set up once at session creation. The `HarnxAgent.sessions` map now stores `Arc<SessionContext>` instead of bare concurrency primitives.

- `new_session`: Fork config once, call `use_agent_by_name` + `use_session`, store `Arc<SessionContext>`.
- `prompt`: Look up `Arc<SessionContext>`, reuse its stored config (no per-prompt fork).
- `cancel`: Works identically via `SessionContext.abort_signal`/`cancel_notify`.
- Lazy resume: Sessions on disk but absent from memory are rebuilt on-demand via `get_or_build_session`.
- Idle reaper: Sessions idle > 15 minutes are evicted (reusing `session_actor.rs` pattern), reaping only when not holding the `prompt_lock`.

This mirrors the NATS worker's correct per-session config pattern from `daemon.rs::execute_session`.
