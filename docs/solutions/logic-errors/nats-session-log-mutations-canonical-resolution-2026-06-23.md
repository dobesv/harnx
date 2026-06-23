---
title: "NATS session-log mutations: retract/edit via EditEntries + canonical resolution"
date: 2026-06-23
category: "logic-errors"
problem_type: logic_error
component: "harnx-nats-session-log"
root_cause: "Raw log entry iteration without mutation resolution allowed retracted messages to be executed by workers"
resolution_type: code_fix
severity: high
tags:
  - session-log
  - mutation
  - EditEntries
  - NATS
  - distributed
  - canonical-resolution
  - testing
plan_ref: "harnx-nats-ha"
---

## Problem

NATS distributed mode introduced queued user messages that clients could retract or edit before worker consumption. The initial implementation allowed retracted messages to be executed by workers because multiple code paths read the raw durable log without applying mutations first. Additionally, an EditEntries YAML serialization bug silently turned edits into deletes.

## Symptoms

- Retracted user messages were still processed by workers (executed despite EditEntries delete)
- Worker integration test `retracted_user_message_is_not_executed_by_worker` failed
- EditEntries with non-serialized replacement strings silently degraded to deletions
- Thin-client history render and final-response extraction showed retracted messages
- Two review cycles (Aristarchus) required to catch all mutation-resolution gaps

## Investigation Steps

1. Initial implementation appended `EditEntries { from, to, replacements: [] }` for retraction
2. Central `apply_log_mutations` function worked correctly in isolation
3. Client and core `reconstruct_state` paths were fixed to apply mutations
4. **Cycle-1 blockers (Aristarchus)**: Worker execution paths still used raw entries:
   - `fold_new_user_messages_since` (mid-turn continuation) did not apply mutations
   - `daemon drain` did not use `reconstruct_state_from_nats` (preserved NATS seqs)
   - Thin-client `extract_final_response` did not apply mutations
5. **Cycle-2 blockers (Aristarchus)**: `edit_user_message` serialized raw string instead of YAML SessionLogEntry:
   - Replacement: `"new text"` instead of proper YAML `SessionLogEntry::Message { ... }`
   - `apply_log_mutations` parsed via `serde_yaml::from_str::<SessionLogEntry>`
   - Parse failed silently → `filter_map` dropped replacement → empty replacements = delete
   - Edit operation silently became retract operation

## Root Cause

**Core issue: Raw-entry iteration without mutation resolution.**

The session log is append-only. Mutations (EditEntries, Rewind) are also entries that modify effective state during replay. Any code path that iterates raw entries without first resolving mutations will see retracted/edited content.

**Affected paths (initially missed):**
- Worker turn-input fold (`fold_new_user_messages_since`)
- Worker drain/resumable check
- Thin-client history render
- Final-response extraction
- `reconstruct_state` in various forms

**Secondary issue: YAML serialization mismatch.**

`EditEntries.replacements` contains YAML strings parsed as `SessionLogEntry`. Raw text strings fail deserialization and are silently dropped, turning non-empty edit into deletion.

## Solution

### 1. Retract/edit via existing EditEntries primitive

Reuse the two-pass-replay mutation primitive instead of inventing a `RetractPending` log variant:

```rust
// Retract (delete)
pub async fn retract_user_message(&self, seq: u64) -> Result<u64> {
    let edit_entry = SessionLogEntry::EditEntries {
        from: seq as usize,
        to: seq as usize,
        replacements: vec![],  // Empty = deletion
    };
    log.append_event_async(&edit_entry).await
}

// Edit (replace)
pub async fn edit_user_message(&self, seq: u64, new_text: String) -> Result<u64> {
    let replacement_entry = SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: MessageRole::User,
        content: MessageContent::Text(new_text),
        timestamp: None,
        fence_token: None,
    };
    // CRITICAL: Serialize as YAML, not raw string
    let replacement_yaml = serde_yaml::to_string(&replacement_entry)
        .context("failed to serialize replacement entry")?;
    let edit_entry = SessionLogEntry::EditEntries {
        from: seq as usize,
        to: seq as usize,
        replacements: vec![replacement_yaml],
    };
    log.append_event_async(&edit_entry).await
}
```

### 2. Canonical-resolution rule

**ANY code path reading the durable log to make a decision MUST first resolve mutations.**

Centralized in `harnx-core/src/session_reconstruct.rs`:

```rust
/// Apply mutation entries (EditEntries, Rewind) to build effective entry stream.
pub fn apply_log_mutations(
    raw_entries: &[(usize, SessionLogEntry)],
) -> Vec<(usize, SessionLogEntry)> {
    // Two-pass replay: mutations modify effective state
}

/// Reconstruct from NATS entries (1-based JetStream seqs).
pub fn reconstruct_state_from_nats(entries: &[(u64, SessionLogEntry)]) -> ReconstructedState {
    let raw_with_seq: Vec<_> = entries
        .iter()
        .map(|(seq, e)| (*seq as usize, e.clone()))
        .collect();
    let effective_entries = apply_log_mutations(&raw_with_seq);
    reconstruct_state_effective(&effective_entries)
}
```

Worker fold path:

```rust
pub(crate) fn fold_new_user_messages_since(
    entries: &[(u64, SessionLogEntry)],
    cursor: Option<u64>,
) -> (Vec<Message>, Option<u64>) {
    // CRITICAL: Apply mutations FIRST
    let raw_with_seq: Vec<_> = entries
        .iter()
        .map(|(seq, e)| (*seq as usize, e.clone()))
        .collect();
    let effective_entries = apply_log_mutations(&raw_with_seq);

    // Now safe to fold
    for (seq, entry) in effective_entries {
        // ...
    }
}
```

### 3. Preserve sequence domain

0-based local seqs vs 1-based NATS JetStream seqs require separate functions:

- `reconstruct_state` — 0-based `enumerate()` seqs
- `reconstruct_state_from_nats` — 1-based JetStream seqs (u64 → usize cast)

Using the wrong function strips/mismatches seqs so EditEntries existence-guards never match.

### 4. Testing pub(crate) functions

In-module `#[cfg(test)] mod tests` required to call `pub(crate)` functions directly:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_new_user_messages_since_with_retracts() {
        // Direct call to pub(crate) fn
        let (messages, _) = fold_new_user_messages_since(&entries, None);
        assert_eq!(messages.len(), 0, "retracted messages must not appear");
    }
}
```

**External test crate tests are ineffective**: they must reimplement logic inline, producing tests that pass even when the actual function is broken.

**Sanity-gate regression tests**: Revert the fix and confirm test fails.

## Why This Works

**EditEntries reuse**: The two-pass-replay primitive already handles mutations correctly. Reusing it ensures consistent semantics across local and distributed modes.

**Canonical resolution**: All log readers go through the same mutation resolver. No path can accidentally see pre-mutation state.

**YAML serialization**: Proper `SessionLogEntry` YAML docs parse correctly. Malformed strings are logged and dropped, but valid entries are preserved.

**Sequence domain preservation**: NATS JetStream seqs start at 1, file-based seqs start at 0. Separate reconstruction functions maintain correct seq semantics for EditEntries range checks.

## Prevention Strategies

**Test cases:**
- Worker integration test: retracted message is NOT executed (verify via mock LLM call count)
- Unit test: `fold_new_user_messages_since` with EditEntries retract
- Unit test: `apply_log_mutations` with EditEntries replace (verify replacement content)
- Sanity-gate: revert fix, confirm test fails

**Best practices:**
- ANY log reader MUST call `apply_log_mutations` or use `reconstruct_state_*` wrapper
- EditEntries replacements MUST be serialized `SessionLogEntry` YAML docs
- Preserve seq domain: use `reconstruct_state_from_nats` for NATS, `reconstruct_state` for file
- In-module tests for `pub(crate)` functions; external tests are insufficient

**Code review checklist:**
- [ ] Does this code path read raw log entries?
- [ ] If yes, does it call `apply_log_mutations` or use reconstruction wrapper?
- [ ] Are EditEntries replacements serialized as YAML SessionLogEntry docs?
- [ ] Is the correct seq domain used (0-based vs 1-based)?
- [ ] Are regression tests sanity-gated (fail when fix reverted)?

## Related Issues

- **PR:** Step 3 commits (a2227b64852, 570e1d81715, 01b43637e79)
- **Related Solution:** [non-destructive-session-mutation-two-pass-replay-2026-05-01.md](non-destructive-session-mutation-two-pass-replay-2026-05-01.md) — Two-pass replay foundation
- **Related Solution:** [stacked-mutation-replay-rposition-2026-05-01.md](stacked-mutation-replay-rposition-2026-05-01.md) — rposition for stacked edits
