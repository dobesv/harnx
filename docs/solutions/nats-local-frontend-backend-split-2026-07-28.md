---
title: "Local NATS Front-End/Back-End Split — Phase 1 Foundation"
date: 2026-07-28
category: "integration-issues"
problem_type: integration_issue
component: "harnx-runtime"
root_cause: "in-process agent loop prevented NATS-mediated orchestration tools from working locally"
resolution_type: code_fix
severity: high
tags:
  - nats
  - transport
  - architecture
  - orchestration
  - process-supervision
  - cancellation
  - security
plan_ref: "nats-frontend-backend-split-phase1"
---

# Solution: Local NATS Front-End/Back-End Split

## Problem

Local execution (TUI, one-shot CLI, serve, ACP) ran the agent loop in-process, bypassing the NATS orchestration layer entirely. Only `agent@cluster` remote refs used NATS. This architectural split prevented local users from benefiting from process isolation, cancellation hardening, and future tool-server-over-NATS work.

## Symptoms

- In-process `run_agent_loop` called directly from TUI/one-shot/serve/ACP front-ends
- ThinClientSession abort path had a race: control publish was fire-and-forget, racing `abort()` teardown
- Worker control subscription could miss a cancel published immediately after activation ack
- ACP servers spawned by the worker as sub-agent backends re-entered local NATS (recursive `SessionActivate`)
- Broker credentials (token) leaked via `/proc/<pid>/cmdline` and inherited into tool/sub-agent child envs

## Architecture: Three-Layer Model

**Option (i) — front-end owns broker lifecycle** was chosen:

1. **Front-end** — relays user ↔ NATS broker (TUI/CLI/serve/ACP). NATS-only; no in-process agent loop.
2. **Orchestrator** — front-end owns the shared broker, spawns worker subprocess. Process group teardown guarantees no orphans.
3. **Back-end** — worker runs agent loop + in-process tools/sub-agents (Phase 1 scope). Phase 2 will migrate tools to NATS.

Why front-end-owns-broker: the front-end lifetime maps to user session lifetime. If TUI exits, the broker should exit. Option (ii) (separate orchestrator process) would add extra process management for no gain.

## Solution

### 1. Shared Broker Manager (`nats_local_server.rs`)

**File locking:** `fs4` flock at `<data_dir>/nats/v1/nats.lock`. Winner owns the broker.

**Atomic metadata:** `ports.json` written via `tempfile::NamedTempFile` + `persist()`, containing `{port, nonce, token}`. Mode 0600 on Unix.

**Start-or-join semantics:** `ensure_shared_server()` tries exclusive lock; winner spawns broker, writes metadata, polls readiness. Losers wait, read metadata, validate nonce via connect-token-flush-reread cycle.

**Stale nonce handling:** If metadata nonce mismatches connection nonce, caller retries lock acquisition.

**Owner-drop cleanup:** `ServerOwner::drop()` kills/reaps child, removes `ports.json` and config file only when nonce matches, unlocks.

**SECURITY — loopback bind + config-file auth:**

```rust
// nats.conf (mode 0600)
host: "127.0.0.1"
port: <dynamic>
jetstream {
    store_dir: "<data_dir>/nats/v1/store"
}
authorization {
    token: "<uuid>"
}
```

Launch: `nats-server -c <config_path>` — token NEVER on argv (prevents `/proc/<pid>/cmdline` leak).

**Never use `--auth` on argv.**

### 2. Reserved Local Cluster Identity (`__local__`)

**Dynamic config interception:** `Config::resolve_nats_server(&self, cluster_key)` returns `Cow<NatsServerConfig>`. Remote keys borrow from YAML; `__local__` returns owned dynamic config via `resolve_local_nats_server_config()`.

**Reserved-key precedence:** User YAML at `nats_servers/__local__.yaml` cannot shadow reserved resolution.

**Env handoff:** `HARNX_NATS_URL` + `HARNX_NATS_TOKEN`. Worker subprocess inherits both; front-end and worker resolve identical identity.

**Authenticated connect path:** Always use `Config::nats_client()` / `nats_jetstream()`. Never bare `async_nats::connect()` — bypasses token/mTLS.

### 3. Worker Supervision (`local_orchestrator.rs`)

**Spawn via `current_exe`:** Uses `harnx worker --cluster __local__ --worker-id local`. Standalone front-ends (`harnx-serve`, `harnx-acp-server`) resolve `HARNX_BIN` or sibling binary.

**Readiness marker:** Worker publishes to `cluster.__local__.worker.ready` (core NATS) after consumer creation. Front-end waits before first activation.

**Worker lock dedup:** `<data_dir>/nats/v1/worker.lock` flock elects single worker. Second front-end joins existing worker (no second spawn).

**Respawn-on-death:** `LocalWorkerSupervisor::ensure()` checks child exit, respawns if owner lost process.

**Teardown:**
- Tokio `kill_on_drop(true)`
- Dedicated Unix process group (`setpgid(0, 0)`)
- Linux `PR_SET_PDEATHSIG(SIGTERM)`
- Process-group SIGTERM + direct kill/reap before releasing worker lock

### 4. Cancellation Fix

**Thin-client abort must flush before teardown:**

```rust
// nats_client_session.rs
if let Some(pending) = pending_cancel.take() {
    publish_control_command(&client, session_id, ControlCommand::Cancel).await?;
    // ^^^ uses Client::flush() internally
}
```

**Worker subscribes-before-ack:**

```rust
// daemon.rs::spawn_control_listener
let subscriber = client.subscribe(ctrl_subject).await?;
client.flush().await?;  // SUB is broker-visible before ack
// THEN message.ack().await
```

**Why flush matters:** Core-NATS control subject is non-durable. A cancel published before worker subscription is lost. `flush()` is the ordering barrier.

**Test:** `cancel_immediately_after_activation_ack_is_not_lost` publishes cancel right after observing work queue removal (activation ack boundary).

### 5. ACP Frontend/Backend Role Split

**Problem:** ACP server spawned by worker as sub-agent backend called `run_thin_turn`, recursively entering local NATS and creating nested `SessionActivate`.

**Fix:** Internal env marker `HARNX_INTERNAL_ACP_ROLE=backend` set by `AcpClient` on spawned children (after configured env, so it overrides).

```rust
// harnx-acp/src/lib.rs
pub const ACP_EXECUTION_ROLE_ENV: &str = "HARNX_INTERNAL_ACP_ROLE";
pub const ACP_BACKEND_ROLE: &str = "backend";
```

**Routing in `HarnxAgent::prompt`:**
- `backend` role → `local_executor::run_local_turn` (in-process)
- `frontend` role (default) → `ThinClientSession::run_turn` (NATS)

**Local executor:** Promoted from test executor to production. Scopes worker-supplied `AgentEventSink`, shares `AbortSignal`, runs `run_agent_loop_with_local_handoff` without NATS publish.

### 6. Security: Credential Scrubbing

**Tools/sub-agents must not inherit broker creds:**

```rust
// harnx-acp/src/client.rs
fn scrub_local_nats_env(command: &mut Command) {
    command
        .env_remove("HARNX_NATS_TOKEN")
        .env_remove("HARNX_NATS_URL");
}

// Same in harnx-mcp/src/client.rs
```

**Worker itself still receives credentials** via env handoff from `LocalWorkerSupervisor`.

### 7. Deferred to Phase 2

- Tool servers over NATS (harnx-toolset trait, instance-scoped subjects)
- Sub-agents over NATS (virtual tool, recursion-depth guard)
- Instance scoping (`instance_id`) + KV tool registration
- SIGCHLD crash detection for tool servers
- Blob spilling to JetStream Object Store
- Cross-session handoff stream-following (thin-client follows session_id change across NATS)
- Sub-agent late-chunk-cancel behavior
- File-log mutation harness rewrite

## Investigation Steps

1. Reviewed existing NATS worker/lease/daemon path — already production for remote clusters.
2. Recon identified `run_agent_loop_with_local_handoff` as the local bypass point.
3. Discovered ThinClientSession abort race via code inspection; wrote test proving fix.
4. Found ACP recursive entry via three failing e2e tests (`interrupt_oneshot_during_sub_agent`, `interrupt_tui_during_sub_agent`, `nested_sub_agent_activity_no_duplicates`).
5. Aristarchus security review flagged broker bind address and argv token leak.

## Gotchas Hit

1. **`#[cfg(test)]` vs runtime seam:** Integration tests in `tests/` don't get crate-level `cfg(test)`. Serve uses `AgentCallFn` injected at runtime; if `Some`, routes to test executor.

2. **Box::pin for recursive futures:** ACP test executor with multiple `tokio::join!` futures each embedding `run_agent_loop_with_local_handoff` overflows default nextest worker stack. Box the future:

```rust
Box::pin(harnx_runtime::run_agent_loop_with_local_handoff(&loop_ctx, input)).await
```

3. **Knope changeset key must be unquoted:**

```yaml
# WRONG
"harnx": minor

# CORRECT
harnx: minor
```

Quoted key fails knope parsing silently.

## Prevention Strategies

**Test coverage:**
- `thin_client_abort_signal_cancels_blocked_worker_and_persists_tombstone` — frontend abort → worker cancel
- `cancel_immediately_after_activation_ack_is_not_lost` — subscribe-before-ack
- `cancel_control_command_serializes_and_is_publishable` — serialization
- Integration tests for shared broker start/join/drop
- Worker respawn + teardown tests

**Code review checklist:**
- [ ] Never pass auth token via argv (use config file)
- [ ] Flush after NATS control publish/subscribe
- [ ] Subscribe before ack for non-durable subjects
- [ ] Scrub `HARNX_NATS_TOKEN`/`HARNX_NATS_URL` from tool/sub-agent child envs
- [ ] Use `Config::nats_client()` / `nats_jetstream()` for authenticated connect
- [ ] Reserved `__local__` key cannot be shadowed by user YAML

**Monitoring:**
- Broker readiness heartbeat on `cluster.__local__.worker.ready`
- Worker lock acquisition via `worker.lock` pidfile

## Related Issues

- **GitHub:** [#1224](https://github.com/dobesv/harnx/issues/1224) — Always use NATS for sub-agents and tool servers
- **Plan:** `nats-frontend-backend-split-phase1`
- **Prior Art:** `nats-ha-lease.md` — KV CAS+TTL lease for worker fencing
- **Code:** `crates/harnx-runtime/src/nats_local_server.rs`, `local_orchestrator.rs`, `nats_client_session.rs`, `nats_worker/daemon.rs`
