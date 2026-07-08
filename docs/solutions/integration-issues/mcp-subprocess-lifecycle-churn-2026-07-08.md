---
title: "MCP subprocess lifecycle churn under ACP prompt loop"
date: 2026-07-08
category: integration-issues
problem_type: logic_error
component: harnx-runtime
root_cause: "Unconditional manager rebuild on every agent activation + rmcp child ownership prevents exit status capture"
resolution_type: code_fix
severity: high
tags:
  - mcp
  - subprocess
  - rmcp
  - lifecycle
  - acp
  - process-wrap
  - manager-reinit
plan_ref: mcp-restarts-fixups
---

## Problem

MCP servers (bash/fs/time/plans) constantly restarted on every ACP prompt, dropping all `Arc<McpClient>` references, closing child stdin, and killing subprocesses. Additionally, MCP subprocess exit status (clean/error/signal) was invisible to the runtime, and ACP subagents failed to capture logs.

GitHub issues: #988, #989, #990.

## Symptoms

- MCP servers restart on each prompt under ACP
- `read_exec_log` fails with "cannot resolve execution_id" — new harnx-mcp-bash spawns with fresh temp log dir each turn
- MCP subprocess exit code/signal unavailable — cannot distinguish clean exit from SIGKILL/OOM
- ACP subagent logs not captured to expected path

## Investigation Steps

1. Traced `Config::reinit_managers_for_agent` — found it calls `McpManager::initialize` which does `clients.clear()`, dropping all `Arc<McpClient>` refs
2. ACP server (harnx-acp-server) activates its agent on *every* prompt → each turn rebuilt managers
3. `run_tool_discovery_blocking` called `invalidate_all_services()` after discovery — fine for multithread TUI, but ACP runs on `new_current_thread()` runtime
4. rmcp 1.7.0: `TokioChildProcess::spawn()` consumes child into transport; `serve_client()` consumes transport — child handle irretrievable for `wait()`
5. Large file `client.rs` churned under agent edits — two agents reverted partially, silently wiping unrelated changes

## Root Cause

**Primary churn source (#988):** `reinit_managers_for_agent` unconditionally rebuilt `McpManager`/`AcpManager` from scratch. Each activation dropped all `Arc<McpClient>` refs, closing stdin and killing subprocesses.

Two order/comparison gotchas:
- `configs()` returns name-sorted — effective-server list must ALSO sort by name, else non-alphabetical YAML order causes spurious rebuilds
- Manual `PartialEq` impl on `McpServerConfig` omitted `hooks` — hooks-only edit silently kept stale hook policy running

**Secondary churn source (#988):** `run_tool_discovery_blocking`'s `invalidate_all_services()` dropped just-connected services on single-threaded ACP runtime.

**Exit status capture (#990):** rmcp's `serve_client(handler, transport)` consumes the transport which owns the child — no way to `wait()` afterward.

**Log capture (#989):** `HARNX_LOG_LEVEL`/`HARNX_LOG_PATH` not explicitly forwarded into child spawn env.

## Solution

### 1. Preserve manager when unchanged

Compute *effective* scoped server set and PRESERVE existing manager `Arc` when unchanged:

```rust
// crates/harnx-runtime/src/config/servers_split.rs

pub fn reinit_managers_for_agent(&self, package_filter: Option<&str>) {
    let effective_mcp = self.effective_mcp_servers(package_filter);
    let effective_acp = self.effective_acp_servers(package_filter);
    
    // ORDER-INSENSITIVE comparison: both lists sorted by name
    let mcp_unchanged = self.mcp_manager.read().unwrap().configs() == effective_mcp;
    let acp_unchanged = self.acp_manager.read().unwrap().configs() == effective_acp;
    
    if mcp_unchanged && acp_unchanged {
        return; // No rebuild needed
    }
    
    // Rebuild only if truly changed
    if !mcp_unchanged {
        self.mcp_manager.write().unwrap().initialize(effective_mcp);
    }
    if !acp_unchanged {
        self.acp_manager.write().unwrap().initialize(effective_acp);
    }
}

impl McpManager {
    fn configs(&self) -> Vec<McpServerConfig> {
        self.clients.keys().cloned().collect() // sorted by name
    }
}
```

**Key fix:** `effective_mcp_servers`/`effective_acp_servers` must `sort_by(|a, b| a.name.cmp(&b.name))` to match `configs()` order.

**Key fix:** Derive `PartialEq` instead of hand-written:

```rust
// Before (broken): manual impl omitted hooks
impl PartialEq for McpServerConfig {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.command == other.command
        // hooks omitted!
    }
}

// After: include all fields automatically
#[derive(PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub hooks: HooksConfig, // now included
    // ... future fields auto-included
}
```

### 2. Gate service invalidation for single-threaded runtime

```rust
// crates/harnx-mcp/src/client.rs

fn should_invalidate_after_discovery() -> bool {
    std::env::var("HARNX_MCP_KEEP_SERVICES_AFTER_DISCOVERY")
        .ok()
        .map(|v| v != "1")
        .unwrap_or(true)
}

pub fn run_tool_discovery_blocking(/* ... */) {
    // ... discovery ...
    if should_invalidate_after_discovery() {
        self.invalidate_all_services();
    }
}
```

harnx-acp-server sets `HARNX_MCP_KEEP_SERVICES_AFTER_DISCOVERY=1` at startup (internal, not user-facing).

### 3. Capture rmcp child exit status (key pattern)

Spawn child yourself, take stdio, build transport directly:

```rust
// crates/harnx-mcp/src/client.rs

use process_wrap::ProcessGroup;
use rmcp::transport::async_rw::AsyncRwTransport;

impl McpClient {
    async fn connect(&mut self, config: &McpServerConfig) -> Result<()> {
        // Abort prior wait task on reconnect
        if let Some(task) = self.child_wait_task.take() {
            task.abort();
        }
        
        // Spawn child ourselves with process group leader
        let mut child = ProcessGroup::leader()
            .command(&config.command)
            .args(&config.args)
            .spawn()?;
        
        // Take stdio (process-wrap 9.1.0 uses METHODS)
        let stdin = child.stdin().take().ok_or("no stdin")?;
        let stdout = child.stdout().take().ok_or("no stdout")?;
        let stderr = child.stderr().take().ok_or("no stderr")?;
        
        // Build transport directly (requires rmcp feature: transport-async-rw)
        let transport = AsyncRwTransport::<RoleClient, _, _>::new(stdout, stdin);
        
        // Keep child wrapper for exit capture
        let child_wrapper: Box<dyn ChildWrapper> = child;
        let last_notice = self.last_notice.clone();
        self.child_wait_task = Some(tokio::spawn(async move {
            let status = child_wrapper.wait().await;
            // Map to notice: 0/SIGTERM => Warning, else => Error
            let notice = match status {
                Ok(s) if s.success() => Notice::warning("MCP server exited cleanly"),
                Ok(s) if s.code() == Some(0) => Notice::warning("MCP server exited"),
                Ok(s) => Notice::error(format!("MCP server exit: {:?}", s)),
                Err(e) => Notice::error(format!("MCP server wait error: {}", e)),
            };
            emit_notice_dedup(&last_notice, notice);
        }));
        
        // serve_client consumes transport
        let handler = McpClientHandler::new(self.clone());
        serve_client(handler, transport).await?;
        
        Ok(())
    }
}
```

**Key insight:** No double-close — stdio ownership moved to transport, child owns only the process.

### 4. Forward log env explicitly

```rust
// crates/harnx-runtime/src/config/servers_split.rs

fn build_child_env(&self) -> Vec<(String, String)> {
    let mut env = Vec::new();
    
    // Forward log config explicitly
    if let Ok(level) = std::env::var("HARNX_LOG_LEVEL") {
        env.push(("HARNX_LOG_LEVEL".into(), level));
    }
    if let Ok(path) = std::env::var("HARNX_LOG_PATH") {
        // Absolutize and preserve {pid} template for children
        let abs_path = dunce::canonicalize(&path).unwrap_or_else(|_| path.into());
        env.push(("HARNX_LOG_PATH".into(), abs_path.to_string_lossy().into()));
    }
    
    env
}
```

Emit notices via `harnx_core::sink::emit_agent_event(AgentEvent::Notice(...))` — propagates through ACP nesting.

## Why This Works

1. **Preserve-manager pattern:** Only rebuild on *actual* config change, not every activation. Sorted comparison prevents spurious rebuilds from YAML reordering.

2. **Derived PartialEq:** Automatically includes all fields — future additions won't silently break equality.

3. **Self-spawned child:** Owns the waitable handle before transport consumes stdio. `JoinHandle` stored/aborted on reconnect prevents lingering tasks.

4. **Explicit env forwarding:** Don't rely on inheritance — absolutize paths, preserve `{pid}` template for per-process expansion.

5. **Notice propagation:** Global/task-local `harnx_core::sink` reachable from any crate; ACP subagent notices bubble up automatically.

## Prevention Strategies

**Code Review Checklist:**
- [ ] Manager rebuild: compute effective set, compare order-insensitively (sorted), preserve when unchanged
- [ ] `#[derive(PartialEq)]` over hand-written impls to avoid field omission
- [ ] Process spawn: own child before transport takes stdio; abort prior wait-task on reconnect
- [ ] Env forwarding: explicit, absolutized paths, template tokens preserved

**Best Practices:**
- Verify `cargo build` after agent hands back large-file edit
- Establish pre-existing failure baseline: stash changes, run tests on base branch
- Gate runtime-specific behavior via internal env flags (document safety rationale)
- Store `JoinHandle` for background tasks; abort on reconnect to prevent leaks

**Test Cases:**
- Manager preserves when server definition order differs (alphabetical vs YAML)
- Manager rebuilds when only hooks change (regression test for omitted-field bug)
- MCP subprocess exit captured as Warning/Error based on exit code
- MCP notices propagate through ACP nesting

**Process meta-lesson:**
Large files (`client.rs`) churn badly under agent edits. Verify `cargo build` after each agent handoff. Establish pre-existing-failure baseline before trusting "these failures are pre-existing."

## Related Issues

- **GitHub:** [#988](https://github.com/dobesv/harnx/issues/988) — MCP servers constantly restarting under ACP
- **GitHub:** [#989](https://github.com/dobesv/harnx/issues/989) — ACP subagent log capture
- **GitHub:** [#990](https://github.com/dobesv/harnx/issues/990) — MCP subprocess exit status
- **Related Solution:** [async-patterns/mcp-server-background-task-supervision-2026-05-25.md](../async-patterns/mcp-server-background-task-supervision-2026-05-25.md) — rmcp RunningService supervision patterns
