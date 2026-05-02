---
title: "Stacked mutation replay uses rposition for duplicate seq ranges"
date: 2026-05-01
category: "logic-errors"
problem_type: logic_error
component: "harnx-session-log"
root_cause: "position() finds first match; 1-to-N edits create multiple entries with same seq requiring last match"
resolution_type: code_fix
severity: high
tags:
  - session-log
  - mutation
  - stacked-edits
  - rposition
  - rust-patterns
plan_ref: "harnx-342-phase2"
---

## Problem

After a 1-to-N edit (single entry replaced by multiple entries), all replacements share the mutation's seq number. A subsequent edit targeting that seq must replace ALL entries with that seq. Using `position()` found only the first match, corrupting replay state.

## Symptoms

- `load_replays_stacked_mutations_reedit_expanded_mutation_seq` test failed
- After editing entry 1 → [1a, 1b], then editing "entry 3" (which is actually the second replacement sharing seq 3), only the first replacement was removed
- Effective log state showed wrong entries: `[1a, 1b, ...]` instead of `[re-edited, ...]`
- Replay logic silently produced incorrect session state with no error message

## Investigation Steps

1. Wrote test `load_replays_stacked_mutations_reedit_expanded_mutation_seq` showing the failure
2. Traced through `build_effective_log_entries` and found the `start_idx`/`end_idx` computation
3. Realized `position()` always returns the first matching element
4. After a 1→2 edit creating `[seq=3, seq=3]`, `position(|seq| seq == 3)` finds index 0, not index 1
5. The range splice would splice `[0..=1]` when it should splice `[0..=1]` for `from` but `[0..=1]` for `to` correctly requires finding the LAST occurrence

## Root Cause

**Asymmetric iterator direction requirement:**

- `from` seq (start of range): Use `position()` — first occurrence is correct because entries are in log order
- `to` seq (end of range): Use `rposition()` — last occurrence needed when multiple entries share `to` seq

When edit_entries with `from: 1, to: 1` creates two replacements, both get the mutation's seq (say, 3). The raw log becomes:
```yaml
seq 0: header
seq 1: user message      ← edited away
seq 2: assistant message
seq 3: edit_entries { from: 1, to: 1, replacements: [user-a, user-b] }
```

After replay, effective_entries has:
```text
(3, Message "user-a")
(3, Message "user-b")   ← same seq!
(2, Message "assistant")
```

A subsequent edit `from: 3, to: 3` must replace BOTH `(3, _)` entries. `position()` finds the first, `rposition()` finds the last.

## Solution

Changed `build_effective_log_entries` in `crates/harnx-runtime/src/config/session.rs`:

**Before:**
```rust
let Some(start_idx) = effective_entries
    .iter()
    .position(|(existing_seq, _)| existing_seq == from)
else { ... };
let Some(end_idx) = effective_entries
    .iter()
    .position(|(existing_seq, _)| existing_seq == to)  // BUG
else { ... };
```

**After:**
```rust
let Some(start_idx) = effective_entries
    .iter()
    .position(|(existing_seq, _)| existing_seq == from)
else { ... };
let Some(end_idx) = effective_entries
    .iter()
    .rposition(|(existing_seq, _)| existing_seq == to)  // FIXED
else { ... };
```

The splice operation `effective_entries.splice(start_idx..=end_idx, ...)` now correctly captures the full block of entries sharing the same seq.

## Why This Works

`rposition()` scans from the end, returning the index of the LAST element matching the predicate. When an edit creates multiple replacements (1→N), they all share the mutation seq as a block. The `to` bound of a subsequent edit must find the end of that block, not its beginning.

This is a **directional asymmetry**: `from` needs the earliest occurrence (forward scan), `to` needs the latest occurrence (backward scan).

## Prevention Strategies

**Test cases:**
- Add test for stacked 1→N edit followed by another edit targeting the mutation seq
- Verify effective entries count matches expected
- Verify message content after replay matches expected

**Best practices:**
- When splicing ranges in collections that may have duplicate keys, consider whether bounds should use `position` or `rposition`
- Document why the asymmetry exists — it's not arbitrary

**Code review checklist:**
- [ ] Does `position` vs `rposition` match the semantic intent (first vs last)?
- [ ] Are there tests for 1→N edit scenarios?
- [ ] Does the code handle multiple entries sharing the same seq?

## Related Issues

- **PR:** #423 — Non-destructive session history editing
- **Related Solution:** [non-destructive-session-mutation-two-pass-replay-2026-05-01.md](non-destructive-session-mutation-two-pass-replay-2026-05-01.md)
