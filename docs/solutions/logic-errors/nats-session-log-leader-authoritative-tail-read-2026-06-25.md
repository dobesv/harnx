---
title: "NATS session-log leader-authoritative tail read for decision points"
date: 2026-06-25
category: "logic-errors"
problem_type: logic_error
component: "harnx-nats-session-log"
root_cause: "Stale read upper bound via stream.info().last_sequence at decision points could miss client edits/retracts due to NATS replication lag"
resolution_type: code_fix
severity: high
tags:
  - session-log
  - NATS
  - distributed-systems
  - replication-lag
  - leader-authoritative
  - jetstream
  - testing
plan_ref: "nats-mid-turn-fresh-read-917"
---

## Problem

The NATS worker's mid-turn injection decision points read the session log tail using `load_events_consistent_async` → `NatsSessionLog::load_events_at_least_async`, which bounds the upper read sequence by `stream.info().state.last_sequence`. This value reflects only the worker's own `after_seq_observer` high-water and can lag behind client writes committed at higher sequences. A client edit/retract committed above the observer's sequence can be missed, causing retracted or stale messages to be injected.

The fold logic (`apply_log_mutations`) was already correct — the bug was purely in the stale read upper bound.

## Symptoms

- Retracted user messages occasionally injected during mid-turn tool rounds
- Client edits arriving just before injection decision not observed
- Bug only reproducible under genuine multi-node NATS replication lag
- Single-node test environments could not reproduce the failure

## Investigation Steps

1. **Identified decision points**: Found 4 sites where worker must observe latest CLIENT writes:
   - `agent_loop.rs:111` — mid-turn injection callback (`build_mid_turn_injection_callback`)
   - `daemon.rs:351` — continuation turn input derivation
   - `daemon.rs:618` — end-of-turn drain reread
   - `daemon.rs:747` — `reconstruct_session_state` activation

2. **Traced stale read path**: All 4 sites used `load_events_consistent_async` → `load_events_at_least_async` → `read_all_from`. The upper bound came from `stream.info().state.last_sequence`.

3. **Analyzed observer semantics**: `load_events_at_least_async` waits for the worker's own `after_seq_observer` high-water. Client writes committed after the observer's tracked sequence are NOT guaranteed visible.

4. **Confirmed fold logic correct**: `apply_log_mutations` and `fold_new_user_messages_since` handle EditEntries correctly. Bug confined to read path upper bound.

5. **Researched async-nats API**: Found `Stream::get_last_raw_message_by_subject(&subject)` routes via `STREAM.MSG.GET last_by_subject` to the STREAM LEADER — always authoritative for latest sequence, no `allow_direct` flag needed.

## Root Cause

`stream.info().state.last_sequence` on a NATS JetStream follower returns the follower's locally replicated high-water mark, which can lag behind the leader's actual latest sequence during replication delay. The worker's `after_seq_observer` only tracks the worker's own published sequences, not client-published sequences.

When a client retract/edit commits at sequence X+1 on the leader, and the follower's `stream.info()` reports last_sequence = X, the old read path stops at X and misses the retract. The real bug requires:

- Multi-node NATS cluster
- `STREAM.INFO` hitting a follower with stale metadata
- Client write committed to leader but not yet replicated

This divergence is impossible on single-node test environments where `STREAM.INFO` always returns the node's authoritative state.

## Solution

Added `NatsSessionLog::load_events_latest_async` that uses leader-authoritative tail discovery:

```rust
pub async fn load_events_latest_async(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
    let stream = self.ensure_stream().await?;
    match stream.get_last_raw_message_by_subject(&self.subject).await {
        Ok(latest) => {
            // Read from start through leader-authoritative latest sequence
            self.read_range(1, latest.sequence).await
        }
        Err(e) => match e.kind() {
            LastRawMessageErrorKind::NoMessageFound => Ok(vec![]),
            _ => Err(e).context("get_last_raw_message_by_subject failed"),
        },
    }
}
```

Key implementation details:

1. **Leader-authoritative discovery**: `get_last_raw_message_by_subject` always reaches the stream leader via `STREAM.MSG.GET last_by_subject` — no replication lag possible.

2. **Empty stream handling**: `NoMessageFound` returns `Ok(vec![])` — clean empty-session semantics.

3. **No stream.info() upper bound**: Reads `1..=latest.sequence` directly, avoiding the stale metadata path entirely.

4. **DRY refactor**: Extracted `read_range(start, last)` helper shared with `read_all_from`.

5. **Thin backend wrapper**: `NatsSessionLogBackend::load_events_latest_async` mirrors existing pattern, no observer argument.

Wired into exactly 4 decision points, replacing the stale reads. Preserved `load_events_consistent_async` / `load_events_at_least_async` unchanged for worker read-your-writes semantics.

## Why This Works

`get_last_raw_message_by_subject` uses JetStream's direct message API (`STREAM.MSG.GET last_by_subject`) which:

- Routes to the STREAM LEADER by definition (leader holds authoritative state)
- Returns the actual latest message for the subject
- Provides sequence number that IS the true latest, not a possibly-stale replica view

By discovering the latest sequence from the leader first, then reading the full range, we guarantee observation of any client write committed before the read — regardless of follower replication state.

## Prevention Strategies

### Pattern/Rule

For decision points that MUST observe the latest CLIENT writes (not just the worker's own), use a leader-authoritative read (`get_last_raw_message_by_subject`), NOT `stream.info().last_sequence` which only reflects the local replica state.

**When to use `load_events_latest_async`:**
- Mid-turn injection decisions
- Activation/reconstruction requiring latest client state
- Continuation input derivation
- Drain decisions before turn completion

**When `load_events_consistent_async` is appropriate:**
- Read-your-writes semantics (worker observing own writes)
- High-water observer tracking already correct

### Testing Strategy: Behavioral + Structural Guard

**Critical testing insight**: A deterministic "fail-on-revert" regression test is IMPOSSIBLE on single-node NATS.

Evidence from async-nats 0.42.0 source:
- `Stream::info()` performs a FRESH `STREAM.INFO` round-trip every call (not cached; `cached_info()` is a separate method)
- harnx builds a fresh `Stream` handle on every read (`ensure_stream` → `get_or_create_stream` round-trips fresh info)
- Once a client retract at X+1 is acked, the OLD path's next fresh `STREAM.INFO` also returns last_sequence ≥ X+1
- Both old and new paths pass any such test — VACUOUS

The real bug requires genuine multi-node `STREAM.INFO`-vs-leader REPLICATION DIVERGENCE, which single-node test servers never exhibit.

**Solution: Option C — documented behavioral + structural guard:**

```rust
/// #917: Behavioral test proving load_events_latest_async reads leader-authoritative tail
/// and honors EditEntries retractions via fold.
///
/// NOTE: Cannot deterministically fail on revert because single-node NATS
/// STREAM.INFO is fresh (not cached) and returns same last_sequence as leader.
/// Real bug multi-node replication lag — see plan note "db530bc1".
#[tokio::test]
async fn load_events_latest_async_reads_leader_authoritative_tail() {
    // 1. Append user message at seq X
    // 2. Append EditEntries retract at seq X+1
    // 3. Call load_events_latest_async()
    // 4. Assert returned entries include X+1
    // 5. Fold with reconstruct_state_from_nats
    // 6. Assert retracted text absent from effective messages
}

/// #917: Structural regression guard for decision-point wiring.
#[test]
fn injection_decision_points_use_leader_authoritative_read() {
    let agent_loop = fs::read_to_string(agent_loop_path).unwrap();
    let daemon = fs::read_to_string(daemon_path).unwrap();
    
    assert!(agent_loop.contains("load_events_latest_async"));
    assert!(!agent_loop.contains("load_events_consistent_async"));
    assert_eq!(daemon.matches("load_events_latest_async").count(), 3);
    assert_eq!(daemon.matches("load_events_consistent_async").count(), 0);
}
```

**Reusable testing guidance**: When a bug depends on distributed-systems timing/replication divergence that the test environment can't reproduce, prefer documented behavioral + structural-guard approach over injecting test-only seams into production. Record WHY the stronger test is infeasible.

### Code Review Checklist

- [ ] For decision points requiring latest CLIENT writes: does code use `load_events_latest_async`?
- [ ] Is `stream.info().last_sequence` avoided for decision-point upper bounds?
- [ ] If adding a new read path: does it need leader-authoritative or read-your-writes semantics?
- [ ] Structural guards: anchored with issue reference comments?

## Related Issues

- **GitHub:** [#917](https://github.com/dobesv/harnx/issues/917) — NATS: mid-turn injection uses non-consistent read
- **Related Solution:** [nats-session-log-mutations-canonical-resolution-2026-06-23.md](nats-session-log-mutations-canonical-resolution-2026-06-23.md) — EditEntries mutation fold (correct, this fix addresses the read path)
- **Related Solution:** [nats-ha-lease.md](../nats-ha-lease.md) — NATS HA worker lease mechanics
