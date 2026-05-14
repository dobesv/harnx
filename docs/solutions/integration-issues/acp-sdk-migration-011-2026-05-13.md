---
title: "Migrating agent-client-protocol SDK from 0.10.4 to 0.11.x (complete API redesign)"
date: 2026-05-13
category: integration-issues
problem_type: integration_issue
component: harnx-acp
root_cause: breaking API redesign with trait-to-builder pattern migration
resolution_type: code_fix
severity: high
tags:
  - rust
  - sdk-migration
  - agent-client-protocol
  - async
  - builder-pattern
plan_ref: acp-sdk-migration
---

## Problem

The `agent-client-protocol` Rust SDK underwent a complete API redesign between versions 0.10.4 and 0.11.x. The core abstractions changed from trait-based implementations to a builder pattern with role marker structs. Projects using the old API experienced widespread compile failures and required significant refactoring to adopt the new patterns.

## Symptoms

- 58+ compile errors in `client.rs` after dependency upgrade
- Missing types: `ClientSideConnection`, `AgentSideConnection` no longer exist
- Trait implementation errors: `acp::Client` and `acp::Agent` are no longer traits
- Schema types not found at crate root (moved to `schema` module)
- Async handler trait bound failures (`Send` requirements)
- Cancel notifications not processed during long-running prompt handlers

## Investigation Steps

1. Reviewed SDK examples in `/home/dobes/.cargo/git/checkouts/rust-sdk-762d172c5e846930/75f5b68/src/agent-client-protocol/`
2. Identified `yolo_one_shot_client.rs` as the canonical example of new builder API
3. Traced schema type re-exports to `agent_client_protocol::schema::*`
4. Discovered dispatch loop blocking issue when `PromptRequest` handlers run long operations inline
5. Found that `spawn_local` is required to offload handlers so `CancelNotification` messages can be processed concurrently

## Root Cause

### 1. API Redesign (Trait → Builder)

**Old API (0.10.4):**
- `acp::Client` and `acp::Agent` were traits implemented on user structs
- `ClientSideConnection::new(impl_client, stdin, stdout, spawn_fn)` created connections
- `AgentSideConnection::new(impl_agent, stdout, stdin, spawn_fn)` for agent side

**New API (0.11.x):**
- `Client` and `Agent` are role marker structs (not traits)
- Builder pattern replaces trait implementations
- `ConnectionTo<Agent>` replaces `ClientSideConnection`

### 2. Dispatch Loop Blocking

Builder handlers run inline on the JSON-RPC dispatch loop. Long-running `PromptRequest` handlers block the loop, causing `CancelNotification` messages to queue unprocessed. Cancel propagation breaks because the notification cannot interrupt the in-progress prompt handler.

### 3. Send Requirements

New builder callbacks must be `Send`. Internal state using `Rc<RefCell<T>>` must migrate to `Arc<tokio::sync::Mutex<T>>`.

### 4. Schema Type Relocation

All schema types moved to `agent_client_protocol::schema::*`. Types no longer re-exported at crate root.

## Solution

### Client-Side Migration

```rust
use agent_client_protocol::schema::*;
use agent_client_protocol::{Agent, Client, ConnectionTo, ByteStreams};

// Build connection with handler callbacks
Client
    .builder()
    .on_receive_notification(
        async |notification: SessionNotification, _cx| {
        // Handle session notifications
        },
        on_receive_notification!(),
    )
    .on_receive_request(
        async |request: RequestPermissionRequest, responder, _cx| {
        // Handle permission requests, call responder.respond()
        },
        on_receive_request!(),
    )
    .connect_with(
        ByteStreams::new(stdin, stdout),
        |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            connection
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            connection
                .send_request(PromptRequest::new(session_id, content))
                .block_task()
                .await?;
            Ok(())
        },
    )
    .await?;
```

### Agent-Side Migration

```rust
use agent_client_protocol::{Agent, Client, Stdio};

Agent
    .builder()
    .on_receive_request_from(
        Client,
        async |request: InitializeRequest, responder, _cx| {
        // Handle initialize, call responder.respond()
        },
        on_receive_request!(),
    )
    .connect_to(Stdio::new())
    .await?;
```

### Offload Long-Running Handlers

```rust
// WRONG: Blocks dispatch loop, cancel notifications queue
.on_receive_request(
    async |request: PromptRequest, responder, _cx| {
        // Long operation here blocks everything
        let result = long_running_work().await;
        responder.respond(...).await;
    },
    on_receive_request!(),
)

// CORRECT: Offload with spawn_local, cancel can be processed
.on_receive_request(
    async |request: PromptRequest, responder, cx| {
        tokio::task::spawn_local(async move {
            let result = long_running_work().await;
            responder.respond(...).await;
        });
    },
    on_receive_request!(),
)
```

### State Migration (Rc → Arc)

```rust
// Old: Not Send-safe
struct OldState {
    data: Rc<RefCell<HashMap<String, Session>>>,
}

// New: Send-safe for builder callbacks
struct NewState {
    data: Arc<tokio::sync::Mutex<HashMap<String, Session>>>,
}
```

### Import Changes

```rust
// Old (0.10.4)
use agent_client_protocol::{CancelNotification, PromptRequest, ...};

// New (0.11.x)
use agent_client_protocol::schema::{
    CancelNotification, PromptRequest, ...
};
// Or glob import:
use agent_client_protocol::schema::*;
```

### Transport Wrappers

```rust
// Client side: wrap async IO
use agent_client_protocol::ByteStreams;
let transport = ByteStreams::new(stdin, stdout);

// Agent side: stdio transport
use agent_client_protocol::Stdio;
let transport = Stdio::new();

// Tokio IO types still need TokioCompat wrapper
use crate::compat::TokioCompat;
let stdin = TokioCompat::new(tokio_stdin);
```

## Why This Works

1. **Builder pattern**: Decouples connection setup from request/response handling. Each callback registers a specific message type handler.

2. **spawn_local offloading**: Keeps the dispatch loop responsive. Cancel notifications process concurrently with prompt handling.

3. **Arc<Mutex> state**: Satisfies `Send` trait bounds required by builder callbacks that may run on different threads.

4. **Schema module**: Centralized type location prevents ambiguity and import conflicts.

## Prevention Strategies

**During SDK Upgrades:**
- Check SDK changelog for breaking changes before upgrading
- Review example code in SDK repository for canonical patterns
- Run `cargo check` immediately after dependency update to surface breakage early

**Handler Design:**
- Default to `spawn_local` for any handler that may block (>10ms operations)
- Never hold locks across await points in handlers unless necessary
- Test cancel propagation explicitly with long-running prompts

**Code Review Checklist:**
- [ ] Are builder callbacks `Send`-compatible?
- [ ] Are long handlers offloaded via `spawn_local`?
- [ ] Are imports updated to `agent_client_protocol::schema::*`?
- [ ] Is cancel propagation tested?

## Related Issues

- **PR:** [#298](https://github.com/dobesv/harnx/pull/298) — Renovate bump of agent-client-protocol
- **Related Solution:** [workflow-issues/tokiocompat-deduplication-2026-05-07.md](../workflow-issues/tokiocompat-deduplication-2026-05-07.md) — TokioCompat wrapper for futures_io traits
