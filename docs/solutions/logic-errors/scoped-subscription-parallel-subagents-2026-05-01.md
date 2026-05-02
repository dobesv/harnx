---
title: "Scoped chunk subscription prevents duplicate sub-agent output in parallel dispatch"
date: 2026-05-01
category: logic-errors
problem_type: logic_error
component: "harnx-acp"
root_cause: "broadcast subscription registered on all clients instead of target client"
resolution_type: code_fix
severity: high
tags:
  - acp
  - concurrency
  - sub-agent
  - event-propagation
  - parallel-dispatch
plan_ref: "issue-420-duplicate-subagent-output"
---

## Problem

Sub-agent activity showed N-fold duplicate outputs when a parent agent dispatched to N sub-agents in parallel. Each sub-agent's events appeared in the parent's display once for every concurrent sub-agent, not just once.

## Symptoms

```text
# Parent agent (aristarchus) dispatches to 4 sub-agents in parallel:
# - urania_session_prompt
# - terpsichore_session_prompt
# - thalia_session_prompt
# - euterpe_session_prompt

# Expected: each sub-agent's output appears once
# Actual: each sub-agent's output appears 4 times

# Example: urania emits "Notice: task started"
# Display shows:
#   [urania] Notice: task started
#   [urania] Notice: task started
#   [urania] Notice: task started
#   [urania] Notice: task started
```

Frequency: 100% reproducible with parallel sub-agent dispatch via `join_all`.

## Investigation Steps

1. Observed duplicate pattern: N concurrent sub-agents → each output repeated N times
2. Traced event flow through `AcpNotificationClient::forward_agent_event`
3. Found `AcpManager::subscribe_chunks()` registered subscription sender on ALL clients via iteration:
   ```rust
   // OLD: registered on ALL clients
   for client in self.clients.read().values() {
       client.set_chunk_forwarder(subscription_id, tx.clone()).await;
   }
   ```
4. With parallel dispatch, each tool call created a subscription and registered its sender on all 4 clients
5. When any client emitted an event, it forwarded to ALL registered forwarders → N receivers got the same event

## Root Cause

`AcpManager::subscribe_chunks()` broadcast subscriptions across all `AcpClient`s in the manager. In parallel tool dispatch:

1. Call for agent-A creates subscription_A, registers tx_A on ALL 4 clients
2. Call for agent-B creates subscription_B, registers tx_B on ALL 4 clients
3. Call for agent-C creates subscription_C, registers tx_C on ALL 4 clients
4. Call for agent-D creates subscription_D, registers tx_D on ALL 4 clients

When agent-A emits an event, `AcpNotificationClient::forward_agent_event` sends it to ALL forwarders registered on that client — including tx_B, tx_C, tx_D. Result: N-fold duplication.

The tool name encodes the agent name as a prefix (`{agent}_session_prompt`), so `find_client_for_tool()` can identify the exact target client before subscribing.

## Solution

Added client-scoped subscription methods that register on a single target client:

**Before:**
```rust
// Broad subscription — registers on ALL clients
pub async fn subscribe_chunks(&self) -> (mpsc::UnboundedReceiver<NestedAcpEvent>, u64) {
    let (tx, rx) = mpsc::unbounded_channel();
    let subscription_id = self.next_subscription_id.fetch_add(1, Ordering::Relaxed);
    // BUG: registers on ALL clients
    for client in self.clients.read().values() {
        client.set_chunk_forwarder(subscription_id, tx.clone()).await;
    }
    (rx, subscription_id)
}
```

**After:**
```rust
// Scoped subscription — registers on ONE client
pub async fn subscribe_chunks_for_client(
    &self,
    client: &AcpClient,
) -> (mpsc::UnboundedReceiver<NestedAcpEvent>, u64) {
    let (tx, rx) = mpsc::unbounded_channel();
    let subscription_id = self.next_subscription_id.fetch_add(1, Ordering::Relaxed);
    client.set_chunk_forwarder(subscription_id, tx).await;  // Single client only
    (rx, subscription_id)
}

pub async fn unsubscribe_chunks_for_client(&self, client: &AcpClient, id: u64) {
    client.remove_chunk_forwarder(id).await;
}
```

**ToolProvider::call_tool update:**
```rust
// Find target client BEFORE subscribing
let client = self.find_client_for_tool(&call.name)?;
let (rx, sub_id) = self.manager.subscribe_chunks_for_client(&client).await;
// ... execute tool ...
self.manager.unsubscribe_chunks_for_client(&client, sub_id).await;
```

## Why This Works

- **Client isolation**: Each subscription registers on exactly one client, eliminating cross-client registration
- **Event deduplication**: Events from agent-A only go to agent-A's subscriber, not to all N parallel subscribers
- **Tool name parsing**: The `{agent}_session_prompt` naming convention enables precise client lookup before subscription
- **Lifecycle symmetry**: `subscribe_chunks_for_client` pairs with `unsubscribe_chunks_for_client` for proper cleanup

## Prevention Strategies

**Test Cases:**

```rust
#[tokio::test]
async fn scoped_subscription_isolates_clients() {
    let manager = AcpManager::new();
    manager.initialize(vec![test_config("alpha"), test_config("beta")]);

    let alpha_client = manager.get_client("alpha").unwrap();
    let beta_client = manager.get_client("beta").unwrap();

    // Subscribe ONLY to alpha
    let (mut alpha_rx, sub) = manager.subscribe_chunks_for_client(&alpha_client).await;

    // Beta receiver should stay empty even when alpha receives events
    // (Install spy on beta's forwarder map to verify nothing registered)
}

#[tokio::test]
async fn broad_subscription_registers_on_all_clients() {
    // Contrast test: verify broad path behavior for regression detection
    let (_broad_rx, sub) = manager.subscribe_chunks().await;
    // Verify subscription registered on ALL clients
}
```

**Code Review Checklist:**

- [ ] Subscription methods scope to target client, not all clients
- [ ] Parallel dispatch paths find client before subscribing
- [ ] Unsubscribe paired with subscribe on same client
- [ ] Tests verify cross-client isolation

**Pattern:**

```text
Find client → Subscribe to that client only → Execute → Unsubscribe from that client
```

## Related Issues

- **GitHub:** [#420](https://github.com/example/harnx/issues/420) — Sub-agent activity showing duplicate outputs
- **Related Solution:** [logic-errors/parallel-tool-dispatch-2026-04-30.md](parallel-tool-dispatch-2026-04-30.md) — Parallel tool dispatch ordering (different issue: emit lifecycle, not subscription scope)
- **Not the same as #231:** Issue #231 is about `config.session` races in ACP server subprocess when multiple sessions receive concurrent prompts on the same server process — a separate, latent concern at the server layer
