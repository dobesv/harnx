---
title: "MCP subprocess lifecycle churn under ACP — per-session context + log filter fix"
date: 2026-07-08
category: integration-issues
problem_type: logic_error
component: harnx-acp-server
root_cause: "Per-prompt config fork/re-scope triggered unconditional manager rebuild + simplelog prefix filter dropped logs"
resolution_type: code_fix
severity: high
tags:
  - mcp
  - subprocess
  - lifecycle
  - acp
  - session-context
  - log-filter
  - rmcp
plan_ref: acp-session-context
last_updated: 2026-07-08
---

## Problem

Three related issues (one root cause, one log filter gotcha, one observability gap):

1. **#988 — MCP servers constantly restarting under ACP:** Every ACP prompt forked the global config, re-scoped the agent, and called `reinit_managers_for_agent` → unconditional `McpManager::new() + initialize()` → `clients.clear()` → dropped all `Arc<McpClient>` → closed child stdin → killed every MCP subprocess each turn. Also broke `read_exec_log` (respawned bash picked fresh temp log dir each turn).

2. **#989 — ACP subagent logs not captured:** `setup_logger(is_server=true)` set simplelog allow-filter to `"harnx::serve"`. simplelog 0.12 matches `target.starts_with(filter)`. Workspace crate targets are `harnx_acp` / `harnx_mcp` / `harnx_runtime` (underscores) — none start with `"harnx::serve"` → ALL serve/ACP logs silently dropped.

3. **#990 — MCP subprocess exit status invisible:** rmcp's `TokioChildProcess` consumes the child; can't `wait()` for exit status.

GitHub issues: #988, #989, #990.

## Symptoms

- MCP tools (bash/fs/time/plans) reconnected on every ACP prompt (visible as startup latency)
- `read_exec_log` failed or returned wrong log path after first prompt
- ACP serve logs never appeared (no startup banner, no `[acp:]`/`[mcp:]` lines)
- MCP subprocess crashes silently invisible to users

## Root Cause

### #988 — Per-prompt config derivation

`harnx-acp-server` serves ONE fixed agent but re-forked config and re-scoped on EVERY prompt:

```
prompt() → fork_prompt_config() → use_agent_by_name() → reinit_managers_for_agent() → McpManager::new() + initialize() → clients.clear()
```

Each `reinit_managers_for_agent` dropped existing `Arc<McpClient>` refs, closing stdin, killing subprocesses.

**KEY INSIGHT:** The NATS worker (`crates/harnx-runtime/src/nats_worker/daemon.rs`) was already correct: clone config ONCE, `use_agent_obj` ONCE per session, then loop turns against the SAME `per_session` Arc. The ACP server was the outlier.

### #989 — simplelog prefix matching + underscore targets

The filter `"harnx::serve"` was intended to match serve-mode logs. But workspace crate targets use underscores (`harnx_acp`, `harnx_mcp`), not `harnx::`. simplelog's `starts_with` match means `"harnx_acp".starts_with("harnx::serve")` → false. Everything dropped.

The env vars (`HARNX_LOG_LEVEL`, `HARNX_LOG_PATH`) WERE inherited correctly — it was the filter, not env.

### #990 — rmcp owns the child

`TokioChildProcess::spawn()` consumes the `Command` and owns the child. stdio moved to transport, but `wait()` unavailable.

## Solution

### 1. Per-session context (SessionContext)

**File:** `crates/harnx-acp-server/src/lib.rs`

Create `SessionContext` owning per-session `GlobalConfig` with its OWN `McpManager`/`AcpManager`:

```rust
pub struct SessionContext {
    pub session_id: String,
    pub config: GlobalConfig,
    pub abort_signal: AbortSignal,
    pub cancel_notify: Arc<tokio::sync::Notify>,
    pub prompt_lock: Arc<tokio::sync::Mutex<()>>,
    last_activity: parking_lot::Mutex<Instant>,
}

impl HarnxAgent {
    pub sessions: HashMap<String, Arc<SessionContext>>,
}
```

**Lifecycle:**
- `new_session()`: build `SessionContext` ONCE, store in HashMap
- `prompt()`: reuse session's config directly — NO per-prompt fork/reinit
- Lazy-rebuild when session resumed from disk but absent in memory (no `load_session` handler)
- Idle reaper (15-min TTL, no hard cap) via `spawn_local` on `LocalSet`
- `should_reap()`: idle-expired AND not-running (never reaps session with in-flight prompt)
- Reaper re-checks `should_reap()` AGAIN while holding the sessions map lock before evicting, closing the TOCTOU gap where a prompt could start between the check and the lock
- `touch()` runs on every activity edge — prompt START, prompt STOP (completion OR cancellation, placed before the cancel early-return), and on a `session/cancel` notification — so a turn that runs longer than the TTL isn't reaped immediately after it finishes or is cancelled
- Reap = drop `Arc<SessionContext>` → drop Config → drop McpManager → child stdin closes → subprocess exits

**`reinit_managers_for_agent` unchanged:** keeps original unconditional-rebuild behavior for TUI/other callers. ACP path simply never calls it per-prompt.

### 2. Log filter default

**File:** `crates/harnx-runtime/src/bootstrap.rs`

Change default filter from `"harnx::serve"` to `"harnx"` (matches all `harnx_*` targets via prefix). `HARNX_LOG_FILTER` still overrides.

Fix `.env` precedence to standard dotenv semantics: the ambient/inherited environment always wins; `.env` only fills variables NOT already set (never clobbers an inherited value). This generalizes the original per-key guard — no logging-var special-case is needed, and it also protects any other operator-set variable. Matches python-dotenv / node dotenv / dotenvy defaults.

### 3. Self-spawned child for exit status

**File:** `crates/harnx-mcp/src/client.rs`

Spawn child ourselves instead of via rmcp:

```rust
// Feature: transport-async-rw in harnx-mcp/Cargo.toml

use process_wrap::CommandWrap;
use process_wrap::ProcessGroup::leader;

let wrap = CommandWrap::new(command).wrap(ProcessGroup::leader());
let mut child = wrap.spawn()?;

// Take stdin/stdout/stderr BEFORE building transport
let stdin = child.stdin.take()?;
let stdout = child.stdout.take()?;
let stderr = child.stderr.take()?;

// Build rmcp AsyncRwTransport (self-owned)
let transport = AsyncRwTransport::<RoleClient, _, _>::new(
    stdout,
    stdin,
    MaxMessageSize::default(),
);

// Retain child handle in background wait-task
let child_wait_task = tokio::spawn(async move {
    let status = child.wait().await?;
    let exit_class = classify_exit(status);
    // Emit AgentEvent::Notice with stderr tail
});
```

**Exit classification (`classify_exit`):**
- code 0 / SIGTERM / SIGINT → Warning (clean shutdown)
- nonzero / SIGKILL / other signal → Error
- none → Error

**Notice deduplication:** 5s window to avoid spam.

**call_tool transport errors:** emit Warning (reconnect in progress) / Error (failed reconnect).

### 4. Notice delivery under ACP

`AgentEvent::Notice` maps to ACP text session/update with prefix (ℹ/⚠/🔴).

**Subtlety:** Synchronous reconnect notices run inside agent-loop task → reach per-turn scoped ACP sink. The DETACHED child-death wait-task emits to GLOBAL sink (tokio task-locals don't cross `spawn`). Under ACP, child death surfaces on NEXT `call_tool` via reconnect path.

**Design choice:** "Lazy" option D (Oracle-recommended) — simpler than per-session long-lived sink plumbing, correct behavior.

## Why This Works

1. **Session-lived managers:** Set up once, reuse per prompt. Matches NATS worker pattern. MCP subprocesses live for session lifetime, not per-prompt.

2. **Prefix filter matches targets:** `"harnx"` prefix matches `harnx_acp`, `harnx_mcp`, `harnx_runtime`. Log inheritance works; filter no longer drops everything.

3. **Self-owned child:** Wait-task captures exit status before transport consumes process. Deduped notices surface to user.

4. **Lazy notice propagation:** Child death surfaces on next tool call when reconnect path triggers. Simpler than long-lived sink plumbing, correct UX.

## Prevention Strategies

**Reference Pattern:**
- "Exactly one config/manager per session, set up once, reused per prompt" — copy NATS worker architecture

**Simplelog Gotcha:**
- Workspace crate targets use underscores (`harnx_foo`), not `harnx::`
-简单日志前缀匹配使用 `starts_with` — 确保 filter 前缀正确

**Code Review Checklist:**
- [ ] Per-prompt path: does it fork/re-scope config? Should it own session-lived state instead?
- [ ] Log filter: does it match actual crate targets (check underscore vs `::`)?
- [ ] Process spawn: do we need exit status? Use self-spawn + AsyncRwTransport, retain child handle.

**Test Cases:**
- MCP client reused across multiple prompts in same session (no reconnect logs)
- ACP serve logs visible (startup banner, tool calls)
- MCP subprocess exit surfaces as Notice with correct severity

## Wrong Turns / Rejected Approaches

### 1. "Preserve-Arc band-aid" in `reinit_managers_for_agent`

**Rejected approach:** Add comparison logic to `reinit_managers_for_agent` comparing effective configs (`existing.configs()` vs `effective`), skip rebuild if unchanged. Added `McpServerConfig`/`AcpServerConfig` `PartialEq` derive.

**Why rejected:** Treats symptom, not root cause. Manager rebuild still happened for OTHER callers. The real fix is ACP not calling `reinit` per-prompt at all.

**Outcome:** Band-aid code dropped. `reinit_managers_for_agent` keeps original unconditional-rebuild behavior. ACP path simply never calls it.

### 2. Per-var env forwarding (`HARNX_LOG_*`)

**Rejected approach:** Add `apply_forwarded_log_env`, `resolve_forwarded_log_path`, forward `HARNX_LOG_LEVEL`/`HARNX_LOG_PATH` explicitly to each child. `Config::forwarded_log_env` absolutized paths, preserved `{pid}` template.

**Why rejected:** Problem was the log FILTER dropping logs, not env inheritance. Children already inherit env+cwd correctly. The per-var forwarding was redundant and did not fix #989.

**Outcome:** Env forwarding code dropped. Log filter fixed instead. Guard added to prevent project `.env` from clobbering inherited log env.

### 3. `HARNX_MCP_KEEP_SERVICES_AFTER_DISCOVERY` flag

**Rejected approach:** Add internal env flag to skip invalidation after discovery.

**Why rejected:** Workaround for symptom, not root cause. Gated runtime behavior with undocumented env var.

**Outcome:** Flag dropped. Session-lived context fixes the root issue.

## Related Issues

- **GitHub:** [#988](https://github.com/dobesv/harnx/issues/988) — MCP servers constantly restarting under ACP
- **GitHub:** [#989](https://github.com/dobesv/harnx/issues/989) — ACP subagent log capture
- **GitHub:** [#990](https://github.com/dobesv/harnx/issues/990) — MCP subprocess exit status
- **Reference Implementation:** `crates/harnx-runtime/src/nats_worker/daemon.rs` — "exactly one worker per session" pattern (correct architecture)
