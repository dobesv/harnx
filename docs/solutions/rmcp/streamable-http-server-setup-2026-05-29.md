---
title: "rmcp Streamable HTTP server setup with graceful shutdown"
date: 2026-05-29
category: "rmcp"
problem_type: integration_issue
component: "rmcp-transport"
root_cause: "non-obvious StreamableHttpServerConfig builder and WithGracefulShutdown select! incompatibility"
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - rmcp
  - http
  - axum
  - graceful-shutdown
  - tokio
  - select!
  - CancellationToken
plan_ref: "harnx-mcp-http-transport"
---

## Problem

Adding Streamable HTTP transport (MCP spec 2025-03-26) to an rmcp-based MCP server alongside existing stdio transport requires non-obvious builder patterns for `StreamableHttpServerConfig`, correct import paths for `NeverSessionManager`, and a spawned supervision pattern for `axum::serve` when background tasks must be supervised via `select!`.

## Symptoms

- **E0639**: Attempting struct literal initialization of `StreamableHttpServerConfig` fails: "cannot create non-exhaustive struct using struct literal"
- **E0277**: `WithGracefulShutdown` does not implement `Future` — cannot use in `tokio::select!` even after `pin!`
- **Host header rejected**: Kubernetes service DNS Host headers rejected by rmcp's default loopback-only allowlist
- **SIGTERM handler panic**: `.expect()` on `signal(SignalKind::terminate())` panics in environments without UNIX signals

## Investigation Steps

1. Attempted direct struct literal for `StreamableHttpServerConfig` — rejected due to `#[non_exhaustive]`
2. Tried `tokio::pin!` on `axum::serve(...).with_graceful_shutdown(...)` — compile error: `Future` not implemented
3. Checked rmcp imports — `NeverSessionManager` lives at full path `rmcp::transport::streamable_http_server::session::never::NeverSessionManager`, not re-exported at crate root
4. Tested with K8s deployment — default allowed_hosts (`["localhost","127.0.0.1","::1"]`) rejects external Host headers
5. Reviewed rmcp source (`tower.rs`): empty `allowed_hosts` Vec means "accept all" (line ~252: `if allowed_hosts.is_empty() { return true }`)

## Root Cause

1. **`#[non_exhaustive]`**: rmcp marks config structs non-exhaustive to allow future field additions without breaking changes — requires builder pattern
2. **`WithGracefulShutdown` type**: axum's `.with_graceful_shutdown()` returns a future-combinator struct, not a `Future` impl itself. The `.await` drives it, but you can't borrow it mutably for `select!`
3. **DNS-rebinding protection**: rmcp's default Host allowlist is loopback-only for security; K8s/Docker deployments need explicit opt-out
4. **Signal handler fragility**: `tokio::signal::unix::signal()` can fail in constrained environments; `.expect()` panics the spawned task

## Solution

### 1. Service setup with builder pattern

```rust
use anyhow::Context;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn run_http(host: String, port: u16) -> anyhow::Result<()> {
    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)      // Stateless for K8s horizontal scaling
        .with_json_response(true)        // Streamable HTTP spec-recommended
        .with_cancellation_token(ct.child_token())
        // Empty allowlist = accept any Host header. Required so external
        // (e.g. Kubernetes) Host values aren't rejected by rmcp's default
        // loopback-only allowlist. Deploy behind a trusted ingress/network.
        .disable_allowed_hosts();
    
    let mcp_service = StreamableHttpService::new(
        || Ok(TimeServer::new()),                    // Factory closure
        Arc::new(NeverSessionManager::default()),    // Session manager wrapped in Arc
        config,
    );
    
    let app = axum::Router::new().nest_service("/mcp", mcp_service);
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("failed to bind {host}:{port}"))?;
    
    spawn_shutdown_handler(ct.clone());
    
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { ct.cancelled().await })
        .await?;
    
    Ok(())
}
```

### 2. Factory closure for non-Clone servers

When the server type isn't `Clone`, use `move` closure with cloned data:

```rust
let factory_dir = plans_dir.clone();
let mcp_service = StreamableHttpService::new(
    move || Ok(PlansServer::new(factory_dir.clone())),
    Arc::new(NeverSessionManager::default()),
    config,
);
```

### 3. Spawned supervision for select! with background tasks

`WithGracefulShutdown` cannot be pinned and used in `select!` directly. Wrap in `tokio::spawn`:

```rust
let shutdown_ct = ct.clone();
let server_handle = tokio::spawn(async move {
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown_ct.cancelled().await })
        .await
});
tokio::pin!(server_handle);

let mut cleanup_handle = tokio::spawn(cleanup_loop(dir.clone(), retention_days));
let mut backoff = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

loop {
    tokio::select! {
        result = &mut *server_handle => {
            cleanup_handle.abort();  // Don't let cleanup outlive server
            result??;
            break;
        }
        result = &mut cleanup_handle => {
            match result {
                Err(e) => {
                    eprintln!("[cleanup] task failed: {e}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Ok(()) => { backoff = Duration::from_secs(1); }
            }
            cleanup_handle = tokio::spawn(cleanup_loop(dir.clone(), retention_days));
        }
    }
}
```

### 4. Graceful signal handler with fallback

```rust
fn spawn_shutdown_handler(ct: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = sigterm.recv() => {}
                    }
                }
                Err(e) => {
                    eprintln!("failed to install SIGTERM handler ({e}); falling back to Ctrl-C only");
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        
        ct.cancel();
    });
}
```

### 5. Preserving existing stdio path when adding HTTP mode

Wrap the entire original stdio code path with minimal restructuring:

```rust
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    
    if args.http {
        run_http(args).await
    } else {
        // Original stdio code verbatim — only indentation changes
        let server = PlansServer::new(plans_dir);
        let transport = rmcp::transport::stdio();
        let service = server.serve(transport).await?;
        // ... rest of original code unchanged
        Ok(())
    }
}
```

## Why This Works

1. **Builder pattern**: `StreamableHttpServerConfig::default()` + `.with_*()` methods sidestep `#[non_exhaustive]` restriction
2. **`tokio::spawn` wrapper**: Transforms `WithGracefulShutdown` into a `JoinHandle<Result<_>>` which IS a `Future` when polled via `&mut handle`
3. **`disable_allowed_hosts()`**: rmcp's dedicated method (added in 1.7) makes intent explicit: empty Vec = accept any Host, appropriate when auth is network-based
4. **`ct.child_token()` + parent `cancelled()`:** Parent cancellation cascades to child token registered with rmcp config, terminating the HTTP server gracefully
5. **Graceful signal handler**: Degrading to Ctrl-C fallback avoids panic in environments without `/proc` or signal support

## Prevention Strategies

**Code Review Checklist:**
- [ ] `StreamableHttpServerConfig` uses builder methods, not struct literal
- [ ] `NeverSessionManager` imported from full path `rmcp::transport::streamable_http_server::session::never`
- [ ] Session manager wrapped in `Arc::new(...)`
- [ ] `axum::serve(...).with_graceful_shutdown(...)` wrapped in `tokio::spawn` before `select!`
- [ ] `CancellationToken::child_token()` passed to rmcp config; parent waited in graceful shutdown
- [ ] Signal handler degrades gracefully on UNIX signal install failure
- [ ] Existing stdio code path wrapped in `if !http { ... }` with minimal restructuring

**Best Practices:**
- Use `.disable_allowed_hosts()` with comment explaining why (network-level auth)
- Report HTTP startup with `eprintln!` showing version, host, port, endpoint
- Enable rmcp feature `transport-streamable-http-server` + add `axum`, `tokio-util`, `tower` deps

**Test Cases:**
- Verify HTTP mode starts and responds to MCP initialize via curl
- Verify stdio mode still works (no regression)
- Verify graceful shutdown on SIGTERM (Unix) and Ctrl-C
- Verify cleanup loop restarts on panic (plans server)

## Related Issues

- **GitHub:** [issue #686](https://github.com/dobesv/harnx/issues/686) — Add Streamable HTTP transport
- **Related Solution:** [mcp-server-background-task-supervision-2026-05-25.md](../async-patterns/mcp-server-background-task-supervision-2026-05-25.md) — Supervision pattern for stdio servers
- **MCP Spec:** Streamable HTTP Transport (2025-03-26 revision)
