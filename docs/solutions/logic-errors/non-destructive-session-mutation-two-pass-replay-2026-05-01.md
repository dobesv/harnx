---
title: "Non-destructive session mutation with two-pass log replay"
date: 2026-05-01
last_updated: 2026-05-01
category: "logic-errors"
problem_type: logic_error
component: "harnx-session-log"
root_cause: "append-only log requires mutation entries interpreted at replay time, not in-place edits"
resolution_type: code_fix
severity: medium
tags:
  - session-log
  - mutation
  - two-pass-replay
  - serde
  - forward-compatibility
plan_ref: "harnx-event-tagging-tui-redesign"
---

## Problem

Session logs needed edit, delete, and rewind capabilities without destructively overwriting history. Direct file modification would lose audit trail, break crash recovery, and prevent undo. Needed a mutation model that preserves the original log entries while allowing the session state to reflect changes.

## Symptoms

- Users wanted to fix typos in previous messages without restarting the session
- Long-running sessions needed ability to rewind to earlier points and branch
- Tool call/result pairs needed integrity protection during edits
- Future log entry variants (added in newer versions) needed graceful handling when opened in older clients

## Investigation Steps

Evaluated three approaches for session mutation:

1. **In-place file edit**: Direct modification of the session file. Rejected because:
   - Loses audit trail
   - Cannot undo
   - Complex recovery if edit is interrupted mid-write
   - Breaks if user manually edits the file

2. **Copy-on-write snapshots**: Each mutation creates a new snapshot file. Rejected because:
   - Proliferates files
   - Complex cleanup
   - Difficult to determine "current" state

3. **Append-only mutation entries** (chosen): Each mutation appends a log entry that describes the edit. On reload, mutations are applied to build the effective session state.

Key insight: sequence numbers must be stable across mutations. If seq is stored in each entry, editing entry 5 to become two entries breaks numbering. Solution: derive seq from YAML document position (0-based index in the multi-document stream).

## Root Cause

Traditional in-place mutation doesn't work for append-only logs. The log is the source of truth, but the effective session state must be computed by replaying mutation entries in order. This requires a two-phase approach:

1. **Pass 1**: Collect all raw (seq, entry) pairs from the YAML document stream
2. **Pass 2**: Apply mutations (EditEntries, Rewind) to build the effective entry list

## Solution

### Mutation Schema

Added two new `SessionLogEntry` variants in `crates/harnx-core/src/session.rs`:

```rust
#[serde(rename = "edit_entries")]
EditEntries {
    /// Inclusive range of entry sequence numbers being replaced.
    from: usize,
    to: usize,
    /// Replacement YAML documents (raw strings, one per replaced entry).
    /// Empty vec = deletion.
    replacements: Vec<String>,
},
#[serde(rename = "rewind")]
Rewind {
    /// All entries with seq > after_seq are excluded from context on replay.
    after_seq: usize,
},
#[serde(other)]
Unknown,
```

### Two-Pass Log Replay

In `crates/harnx-runtime/src/config/session.rs`:

```rust
fn load_from_log(config: &Config, name: &str, path: &Path, content: &str) -> Result<Session> {
    let raw_entries = collect_raw_log_entries(content, name)?;
    let mut session = replay_log_entries(&raw_entries, name)?;
    session.log_entry_count = raw_entries.len();
    // ... rest of initialization
}

fn collect_raw_log_entries(content: &str, name: &str) -> Result<Vec<(usize, SessionLogEntry)>> {
    serde_yaml::Deserializer::from_str(content)
        .enumerate()
        .map(|(seq, document)| {
            let entry = SessionLogEntry::deserialize(document)
                .with_context(|| format!("Invalid log entry #{} in session {}", seq, name))?;
            Ok((seq, entry))
        })
        .collect()
}

fn build_effective_log_entries(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
) -> Vec<(usize, SessionLogEntry)> {
    let mut effective_entries = Vec::new();

    for (seq, entry) in raw_entries {
        match entry {
            SessionLogEntry::Rewind { after_seq } => {
                // Validate: after_seq must be < current seq
                // Validate: after_seq must exist in effective_entries
                // Remove all entries with seq > after_seq
                effective_entries.retain(|(s, _)| *s <= *after_seq);
            }
            SessionLogEntry::EditEntries { from, to, replacements } => {
                // Validate: from and to exist in effective_entries
                // Validate: range [from, to] is contiguous
                // Parse replacement YAML strings into entries
                // Splice replacements into effective_entries
                let parsed: Vec<_> = replacements.iter()
                    .filter_map(|r| serde_yaml::from_str(r).ok())
                    .map(|e| (*seq, e))
                    .collect();
                effective_entries.splice(start_idx..=end_idx, parsed);
            }
            SessionLogEntry::Unknown => {} // Skip unknown future variants
            _ => effective_entries.push((*seq, entry.clone())),
        }
    }
    effective_entries
}
```

### Sequence Numbers Derived from Position

Seq is never stored in the entry itself. It's derived from the document position in the YAML stream:

```rust
// In Session struct:
#[serde(skip)]
pub log_entry_count: usize,  // Tracks total documents for next_seq()

pub fn next_seq(&self) -> usize {
    self.log_entry_count
}
```

This guarantees stability: even if an edit replaces entry 5 with 3 entries, they all get seq of the mutation entry, not fractional numbers. The original entry 5 still exists in the raw log at position 5.

### Unknown Variant for Forward Compatibility

```rust
#[serde(other)]
Unknown,
```

The `#[serde(other)]` attribute causes unknown `type:` values to deserialize to `Unknown` instead of failing. During replay, `Unknown` entries are skipped. This allows:

- Old clients opening logs with new entry types (graceful degradation)
- Future schema evolution without breaking existing deployments
- Unknown `type:` tag values deserialize to `Unknown` instead of failing (field-level typos or malformed documents still return an error)

### MutationNotice Ordering

Initially, `run_command` pushed `MutationNotice` to the transcript, then called `reconcile_transcript_after_command`, which cleared the transcript. The notice was destroyed before the user could see it.

Fix: push the notice *after* reconciliation completes:

```rust
// In crates/harnx-tui/src/input.rs run_command:
self.reconcile_transcript_after_command(prev_session, prev_agent, line);
if !clean.is_empty() {
    if is_mutation_command {
        self.app.transcript.push(TranscriptItem::MutationNotice(clean));
    }
    // ...
}
```

### Non-Obvious Decisions

1. **Empty replacements = delete**: `EditEntries { from: 3, to: 5, replacements: [] }` deletes entries 3-5. The empty vec is a valid edit that removes content without a separate `DeleteEntries` variant.

2. **Seq 0 header protection**: The first YAML document is always a `Header` entry with metadata (model, agent, etc.). `.edit` and `.delete` commands must reject `from == 0` to prevent corrupting the session structure.

3. **Rewind bounds validation**: `rewind_session(after_seq)` validates `after_seq < session.log_entry_count` *before* appending the `Rewind` entry. Without this, `.rewind 999999` would append silently and only warn on reload.

4. **Replacement seq assignment**: All replacements from an `EditEntries` get the seq of the mutation entry itself, not their original positions. This makes replacements addressable as a block for further edits.

### Stacked Mutation rposition Fix (Phase 2)

When a 1→N edit creates multiple replacements, they all share the mutation's seq number. A subsequent edit targeting that seq must replace ALL entries with that seq. Using `position()` found only the first match; changed to `rposition()` for the `to` bound:

```rust
// In build_effective_log_entries:
let Some(start_idx) = effective_entries
    .iter()
    .position(|(existing_seq, _)| existing_seq == from)  // first occurrence
else { ... };
let Some(end_idx) = effective_entries
    .iter()
    .rposition(|(existing_seq, _)| existing_seq == to)   // last occurrence!
else { ... };
```

This asymmetry is critical: `from` needs the first matching entry (forward scan), `to` needs the last matching entry (backward scan). Without `rposition`, stacked edits would splice only part of the duplicated-seq block.

See [stacked-mutation-replay-rposition-2026-05-01.md](stacked-mutation-replay-rposition-2026-05-01.md) for full details.

### Tool Result Validation for No-ID Positional Matching (Phase 2)

Some LLM providers omit `tool_call_id` in results, relying on positional correspondence. Validation now supports three cases:

1. **All results have IDs** → validate by ID matching
2. **All results lack IDs** → validate by count matching (positional)
3. **Mixed presence** → reject as ambiguous

```rust
if missing_result_ids == results.len() {
    // All absent: positional matching
    if results.len() != calls.len() {
        bail!("count mismatch for positional matching");
    }
    continue;
}
if missing_result_ids > 0 {
    // Mixed: ambiguous
    bail!("mixes IDs with missing IDs");
}
// All present: ID matching
```

See [tool-result-validation-no-id-positional-2026-05-01.md](tool-result-validation-no-id-positional-2026-05-01.md) for full details.

## Why This Works

**Append-only guarantees audit trail**: Every mutation is recorded. The original entries exist in the log forever. History can be reconstructed at any point.

**Two-pass replay isolates concerns**: Raw log collection doesn't know about mutations. Replay logic only processes mutations. Clean separation of parsing from interpretation.

**Position-derived seq is stable**: Even after edits, the original seq numbers remain valid as positions in the raw log file. An edit at position 5 doesn't shift positions of other entries.

**Unknown variant enables evolution**: Adding new entry types in future versions won't break old clients. They'll see `Unknown` and skip, rather than failing to deserialize entirely.

## Prevention Strategies

**Test cases:**
- Unit tests for `build_effective_log_entries` with overlapping mutations
- Integration tests for `.edit`, `.delete`, `.rewind` commands in TUI
- Test that `Unknown` entries are gracefully skipped
- Test that seq 0 header protection rejects edits

**Best practices:**
- Always use `serde_yaml::Deserializer` for multi-document parsing (not string split)
- Validate mutation bounds before appending entries
- Re-run session load after any mutation to verify effective state

**Code review checklist:**
- [ ] Does the mutation command validate bounds?
- [ ] Does the mutation command protect seq 0?
- [ ] Does the mutation notice survive transcript reconciliation?
- [ ] Are unknown entry types handled gracefully?

## Related Issues

- **Jira:** #342 — Non-destructive session editing
- **Jira:** #396 — Sequence numbers for transcript items
- **PR:** [harnx-event-tagging-tui-redesign] — Full implementation of mutation commands
- **Phase 2 fixes:**
  - [stacked-mutation-replay-rposition-2026-05-01.md](stacked-mutation-replay-rposition-2026-05-01.md) — rposition for stacked mutations
  - [tool-result-validation-no-id-positional-2026-05-01.md](tool-result-validation-no-id-positional-2026-05-01.md) — No-ID positional matching
