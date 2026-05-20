---
title: "MCP stdio proxy pattern using rmcp"
date: 2026-05-19
category: "rmcp"
problem_type: integration_issue
component: "rmcp-transport"
root_cause: "missing RunningService lifetime management and ClientHandler default behavior"
resolution_type: code_fix
severity: high
tags:
  - mcp
  - rmcp
  - proxy
  - stdio
  - subprocess
  - lifetime
  - deadlock
plan_ref: "harnx-mcp-hooks-proxy"
---

# MCP stdio proxy pattern using rmcp

## Problem

Building a transparent stdio MCP proxy that wraps a child subprocess MCP server requires careful handling of rmcp's `RunningService` lifetime and `ClientHandler` default behaviors. Missing these causes transport closure and root-path corruption.

## Symptoms

```text
- Child process exits immediately after handshake completes
- Proxy returns empty tool list despite child having tools
- Child MCP server ignores CLI `--root` flags and uses empty roots
- Deadlock when calling `peer.list_roots()` from `initialize` handler
```

## Investigation Steps

1. Built proxy using `rmcp::transport::stdio()` for server side and `TokioChildProcess::spawn()` for client side
2. Noticed child process terminated unexpectedly after initialization
3. Traced to `RunningService` being dropped when `serve_client()` returned
4. Found `ClientHandler::list_roots` default returns `Ok(ListRootsResult::default())` (empty OK)
5. Child servers interpret empty roots response as "replace CLI roots with empty set"
6. Attempted to call `peer.list_roots()` from `initialize` to forward roots — blocked forever

## Root Cause

### 1. RunningService lifetime

`RunningService<RoleClient, H>` manages the transport connection to the child process. When dropped, it closes the transport and the child process terminates. The `Peer` obtained from `service.peer()` is NOT sufficient to keep the connection alive.

```rust
// WRONG: service dropped, transport closes
let service = rmcp::service::serve_client(handler, transport).await?;
let peer = service.peer().clone();
// service goes out of scope -> child connection dies
```

### 2. ClientHandler::list_roots default

Default implementation returns `Ok(ListRootsResult::default())` — an empty list that signals "no roots available" rather than "method not supported". Child servers that invoke `list_roots` on the proxy (acting as client) interpret this as "replace my CLI `--root` flags with this empty set".

### 3. list_roots deadlock from initialize

The MCP handshake sequence is:
1. Client sends `initialize` request
2. Server returns capabilities
3. Client sends `initialized` notification

Calling `peer.list_roots()` from inside the `initialize` handler creates a circular dependency: the outer client's handshake hasn't completed, so the peer isn't ready to make outgoing requests.

## Solution

### 1. Store RunningService alongside Peer

Keep the `RunningService` in the server state to prevent premature drop:

```rust
struct HooksProxyServerInner {
    child_peer: RwLock<Option<Peer<RoleClient>>>,
    child_service: RwLock<Option<RunningService<RoleClient, ChildClientHandler>>>,
    // ... other fields
}

async fn initialize_child(&self) -> Result<(), ErrorData> {
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(...)?;
    
    let service = rmcp::service::serve_client(handler, transport).await?;
    let peer = service.peer().clone();
    
    // Store BOTH peer and service
    *self.inner.child_peer.write() = Some(peer);
    *self.inner.child_service.write() = Some(service);
    
    Ok(())
}
```

### 2. Return method_not_found from list_roots

Override `list_roots` to return `method_not_found` error, forcing child servers to fall back to CLI roots:

```rust
impl ClientHandler for ChildClientHandler {
    async fn list_roots(
        &self,
        _cx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        // Return error so child falls back to CLI --root flags
        Err(ErrorData::method_not_found::<
            rmcp::model::ListRootsRequestMethod,
        >())
    }
}
```

This works because harnx-mcp-bash's `ensure_roots_initialized` catches errors from `refresh_roots` and falls back to CLI roots if CLI roots are non-empty.

### 3. Never call peer methods from initialize

Defer any peer calls until after `on_initialized` fires, or avoid entirely. For proxy use cases where child servers accept CLI roots, returning `method_not_found` from the proxy's client handler is simpler and avoids timing issues.

## Why This Works

**RunningService lifetime**: Storing `RunningService` in a persistent field ensures the transport stays open for the lifetime of the proxy. The Arc-wrapped inner struct pattern prevents both `Peer` and `RunningService` from being dropped.

**method_not_found pattern**: Child MCP servers that support CLI `--root` flags check for `list_roots` failures and fall back to CLI roots. Returning `method_not_found` explicitly signals "I don't support this method" rather than "I support it but have no roots".

**Avoiding initialize deadlock**: By not calling any peer methods during initialization, the handshake completes cleanly. The proxy can forward tool calls immediately after.

## Prevention Strategies

**Tests:**
- Add integration test verifying child process stays alive after handshake
- Add test verifying child CLI roots are preserved when proxy returns method_not_found
- Add test verifying proxy fails gracefully without the RunningService stored

**Code Review Checklist:**
- [ ] Is `RunningService` stored in long-lived state?
- [ ] Does `ClientHandler::list_roots` return appropriate value for use case?
- [ ] Are any peer methods called from `initialize` handler?

**Best Practices:**
- Always store `RunningService` when using `serve_client` with subprocess transport
- Prefer `method_not_found` over empty OK for methods you don't truly support
- Test proxy with child servers that have CLI configuration
