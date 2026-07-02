---
title: "Remote session shared-seq trap: group-aware EditEntries for worker-migrated headerless sessions"
date: 2026-07-01
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime/remote_session_ops"
root_cause: "Worker header migration creates shared physical seq between Header and first user; naive per-seq EditEntries drops Header or mis-targets deletes/rewinds"
resolution_type: code_fix
severity: high
tags:
  - remote-session
  - NATS
  - JetStream
  - EditEntries
  - session-log
  - header-migration
  - shared-seq
  - logical-index
  - physical-seq
plan_ref: "p915-remote-tui-wiring"
---

## Problem

Remote NATS sessions require edit/delete/rewind/resume operations to work identically to local sessions, but remote sessions use an append-only JetStream log with physical seqs while local sessions use document indices. A subtle trap emerges: worker header migration creates entries that **share a single physical JetStream seq**, and naive per-seq mutations silently corrupt sessions by dropping Headers or mis-targeting operations.

## Symptoms

- Editing the first user turn of a worker-migrated session **drops the Header**, making the session invalid
- Deleting a logical range mis-targets entries when physical seq order diverges from logical order after edits
- Rewind `{after_seq}` truncates incorrectly, deleting wrong entries
- `from > to` no-ops or delete operations hit unintended entries
- Tests pass with hand-seeded Headers but fail with realistic worker-migrated fixtures

## Investigation Steps

1. Observed s6 exact-set delete/rewind regressions after production fix for seq-only targeting
2. Traced realistic 2-turn remote session raw/effective shape after worker migration:
   - raw js1 = U1 `delegate over nats`
   - raw js2 = header-migration `EditEntries{from:1,to:1,replacements:[Header,U1 clone]}`
   - raw js3 = A1, js4 = U2, js5 = A2
3. Effective active window before edit showed **Header and U1 both at physical seq 2**
4. `edit_remote_message_range(1,1)` stored only `js_seq=2`; `thin.edit_user_message(2, ...)` appended `EditEntries{from:2,to:2,replacements:[edited U1]}`
5. Resolver semantics: `from=to=2` spans both Header and U1, replaces both with single user replacement → **Header gone**
6. Delete/rewind similarly mis-targeted when logical order diverged from physical seq order

## Root Cause

**Two intertwined issues:**

1. **Shared-physical-seq trap:** Remote sessions are born headerless. Thin client appends first user before worker activation. Worker migrates by appending ONE `EditEntries{replacements:[Header, cloned users]}`. All replacements **inherit the mutation's single physical js_seq**. Header and first user share seq 2.

2. **Seq-only exact-set model:** `exact_set_delete_mutations` emitted one `EditEntries{seq,seq,[]}` per physical seq, assuming one logical entry per seq. This assumption is **false** for worker-migrated sessions where Header+U1 share seq. The resolver (`apply_log_mutations` in harnx-core) resolves `EditEntries{from:s,to:s}` as `position(seq==s)..=rposition(seq==s)`, splicing the entire span containing **both** entries.

**Consequences:**
- Edit of first user: `EditEntries{2,2,[edited U1]}` replaces Header+U1 → Header dropped
- Delete when physical order != logical order: targets wrong entries, `from>to` no-ops, or deletes unintended rows
- Physical Rewind truncation: wrong boundary

**Additional constraint:** `active_context_window` (entries after most-recent Header|Compress boundary) is for display/dispatch/protection ONLY. Logical indices **must not** enter reconstruct/fold/turn-derivation code paths.

## Solution

### Two Seq Domains

- **Logical index:** Position within `active_context_window` (Header = logical 0, Compress excluded)
- **Physical seq:** JetStream `js_seq` (1-based u64)

Display, dispatch, and protection code use LOGICAL indices. JetStream mutations (`EditEntries`, `Rewind`) use PHYSICAL seqs. **Never mix.**

### Group-Aware Mutation Construction

For any physical seq shared by multiple effective rows, `EditEntries` replacements must preserve non-targeted siblings in effective order.

**Edit operation:**
1. Identify target by logical row (not first match on seq)
2. Target the `role.is_user()` member of the shared-seq group
3. Re-emit all group members with only the target edited:
   ```rust
   append_session_mutation_batch_cas(&thin, |state| {
       let group = state.logical_targets.iter()
           .filter(|t| t.js_seq == target_seq)
           .collect::<Vec<_>>();
       let replacements: Vec<String> = group.iter().map(|t| {
           if t.entry.role().is_user() && t.entry.text() == target_text {
               serde_yaml::to_string(&edited_entry)?
           } else {
               serde_yaml::to_string(&t.entry)?
           }
       }).collect();
       Ok(vec![SessionLogEntry::EditEntries {
           from: target_seq,
           to: target_seq,
           replacements,
       }])
   }).await?;
   ```

**Delete operation:**
1. Build `physical_seq -> [(logical_index, entry_yaml)]` map in effective order
2. For each targeted physical seq, emit:
   ```rust
   SessionLogEntry::EditEntries {
       from: seq,
       to: seq,
       replacements: group_members.iter()
           .filter(|(idx, _)| !targeted_logical_indices.contains(idx))
           .map(|(_, yaml)| yaml.clone())
           .collect(),
   }
   ```
3. Empty replacements when whole group targeted
4. Bail with error if group members are non-contiguous (gap in middle unsupported)

**Rewind operation:**
1. Compute logical SUFFIX to delete
2. Reuse same group-aware deletion (append-only, no physical `Rewind`)
3. Append per-seq mutations under ONE CAS chain

**CAS batch pattern:**
```rust
async fn append_session_mutation_batch_cas<F>(
    thin: &ThinClientSession,
    build_entries: F,
) -> Result<()>
where
    F: Fn(&RemoteRenderState) -> Result<Vec<SessionLogEntry>>,
{
    let log = NatsSessionLog::new(thin.jetstream().clone(), thin.session_id().to_string());
    loop {
        let state = load_remote_session_for_render(thin).await?;
        let entries = build_entries(&state)?;
        if entries.is_empty() {
            return Ok(());
        }
        let mut expected_last = state.last_seen_stream_seq;
        let mut should_retry = false;
        for entry in &entries {
            match log.append_event_with_expected_last_sequence_async(entry, expected_last).await {
                Ok(seq) => expected_last = seq,
                Err(err) if is_stream_advanced_error(&err) => {
                    should_retry = true;
                    break;
                }
                Err(err) => return Err(err),
            }
        }
        if should_retry {
            continue;
        }
        return Ok(());
    }
}
```

### Resume Parity

Remote pre-render reuses local `replay_log_entries_for_external` (tool-turn folding + compaction markers). Overlay same active-window logical numbering so resumed rows are targetable by the same logical seqs.

## Why This Works

**Group-awareness:** Preserves siblings in shared-seq groups (Header+U1) when targeting only one member. Header survives editing first user turn.

**Logical targeting:** Users operate on logical indices (visible transcript positions). Code maps to physical seqs with full group context before emitting mutations.

**Batch CAS:** All mutations for one operation emit under single optimistic concurrency chain. On stale, reload + rebuild + retry ensures consistent snapshot.

**Contiguity check:** Rejects non-contiguous groups early (should never happen in practice with worker migration pattern).

## Prevention Strategies

**Test fixtures:**
- Use REALISTIC worker-migrated headerless fixtures: `ThinClientSession::run_turn` then worker-activated
- NOT hand-seeded headers (they mask off-by-one AND header-drop bugs)
- Always assert HEADER SURVIVAL after editing/deleting first turn of migrated session

**Test cases:**
- Edit first user turn of worker-migrated session → Header survives
- Delete range across logical indices where physical seq order diverges → targets correct entries
- Rewind after edit → truncates at correct boundary
- Resume remote session → logical indices match pre-render, editable

**Best practices:**
- Build `physical_seq -> [(logical_index, entry)]` map before any `EditEntries` construction
- Always preserve non-targeted siblings in shared-seq groups
- Emit all mutations for one user operation in single CAS batch
- Never pass logical indices to reconstruct/fold/turn-derivation code

**Code review checklist:**
- [ ] Does this mutation handle shared-seq groups?
- [ ] Are Header+U1 preserved when editing first user turn?
- [ ] Is group contiguity verified before emitting mutations?
- [ ] Are mutations batched under single CAS chain?
- [ ] Do test fixtures use realistic worker-migration, not hand-seeded headers?

## Related Issues

- **PR:** [#915](https://github.com/example/harnx/pull/915) — Group-aware remote edit/delete/rewind
- **Commit:** d8b0f3597 — fix(runtime): group-aware remote edit/delete/rewind preserves shared-seq siblings
- **Related Solution:** [nats-session-log-mutations-canonical-resolution-2026-06-23.md](nats-session-log-mutations-canonical-resolution-2026-06-23.md) — EditEntries mutation fold and canonical resolution
- **Related Solution:** [non-destructive-session-mutation-two-pass-replay-2026-05-01.md](non-destructive-session-mutation-two-pass-replay-2026-05-01.md) — Two-pass replay foundation
