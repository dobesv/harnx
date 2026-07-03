---
title: "apply_log_mutations skip-branch testing: non-vacuous replay-order case and shared helper pattern"
date: 2026-07-02
category: "logic-errors"
problem_type: logic_error
component: "harnx-core/session_reconstruct"
root_cause: "Test vacuum: from>to EditEntries trips invalid-range guard before reaching not-in-replay-order branch; effective seqs are NOT monotonic after splicing"
resolution_type: test_fix
severity: medium
tags:
  - session-log
  - mutation
  - EditEntries
  - testing
  - non-vacuous-test
  - skip-branch
plan_ref: "nats-edge-case-tests"
---

## Problem

`apply_log_mutations_with_name` has 5 skip branches (log::warn! + continue, never errors/panics) for invalid mutations. A test using `EditEntries { from: 3, to: 2 }` to exercise the "not in replay order" branch was VACUOUS — tripped the `from > to` invalid-range guard (line 79) and never reached the `start_idx > end_idx` branch (line 107).

## Symptoms

- Test passed but exercised wrong guard
- Review caught that `from: 3, to: 2` fails at line 79, not line 107
- Line 107 branch appeared unreachable without deeper analysis

## Investigation Steps

1. Traced guard order: `from > to` checked before position lookups
2. Analyzed effective seq stream structure after EditEntries splices
3. Found: after `EditEntries { from: 1, to: 1, replacements: [...] }`, effective stream has entries with non-monotonic seqs
4. Constructed concrete case:
   - seq1: user("first")
   - seq2: user("second")
   - seq3: EditEntries { from: 1, to: 1, replacements: [user("first replacement one")] }
   - seq4: EditEntries { from: 2, to: 3, replacements: [edited_user] }
5. At seq4: effective before edit is `[(3, "first replacement one"), (2, "second")]`
   - from=2, to=3 → passes `from <= to` guard
   - position(2) = 1, rposition(3) = 0
   - start_idx=1 > end_idx=0 → line 107 fires

## Root Cause

**Effective seqs are NOT strictly monotonic** after EditEntries splices replacements that inherit the mutation's seq. Replacements splice into the middle of the stream, creating out-of-order seq positions.

Test using `from > to` is vacuous because it hits the invalid-range guard first. Real reachable case requires VALID `from <= to` where seq positions become inverted in effective stream.

## Solution

Replaced vacuous test with genuine line-107 coverage:

**Test:** `edit_entries_not_in_replay_order_is_skipped`
```rust
let effective = apply_log_mutations(&[
    (1, user_message("first")),
    (2, user_message("second")),
    (3, SessionLogEntry::EditEntries {
        from: 1,
        to: 1,
        replacements: vec![to_yaml(&user_message("first replacement one"))],
    }),
    (4, SessionLogEntry::EditEntries {
        from: 2,
        to: 3,  // from <= to, but position(2)=1 > rposition(3)=0
        replacements: vec![edited_user_replacement_yaml()],
    }),
]);
```

**Added helper:** `to_yaml(entry: &SessionLogEntry) -> String` in test module (line 809) to cut serde_yaml boilerplate for replacement YAML construction.

**Shared assertion helper:** `assert_edit_is_skipped(edit: SessionLogEntry)` (line 863) asserts effective stream unchanged — non-vacuous, would fail if guard removed.

## Why This Works

**Correct branch exercised:** `from: 2, to: 3` passes line-79 guard, allowing execution to reach line-107 where inverted positions trigger the skip.

**Non-vacuous assertion:** If line-107 guard were removed, `splice(1..=0, ...)` would run on invalid inclusive range and panic — test would catch regression.

**Helper reduces boilerplate:** `to_yaml()` used in touched tests; single source for replacement YAML construction.

## Prevention Strategies

**Test cases:**
- Verify guard-order dependencies before writing skip-branch tests
- For position-inversion branches, construct realistic effective streams with non-monotonic seqs
- Use shared `assert_edit_is_skipped` helper for all skip-branch tests (non-vacuous baseline)
- Sanity-gate: confirm test would fail if targeted guard removed

**Best practices:**
- Analyze what the effective stream looks like BEFORE writing mutation tests
- EditEntries splices inject entries with mutation seq into stream middle
- Shared test helpers (assert_edit_is_skipped, to_yaml) cut boilerplate and enforce consistency

**Code review checklist:**
- [ ] Does skip-branch test actually reach the targeted line?
- [ ] Is the test case non-vacuous (would fail if guard removed)?
- [ ] For position-based guards, is the effective stream structure realistic?

## Related Issues

- **Plan:** nats-edge-case-tests — Part 1 unit tests for apply_log_mutations skip branches
- **File:** `crates/harnx-core/src/session_reconstruct.rs` lines 79-135 (guard logic), 863-949 (tests)
- **Process note:** Part 2/3 (mid-turn injection, Rewind worker) already covered by NATS integration tests:
  - `mid_tool_round_user_message_is_injected_once_into_same_turn` (nats_worker.rs:477)
  - `retracted_mid_tool_round_message_is_not_injected` (nats_worker.rs:1162)
  - `remote_rewind_appends_mutation_without_truncating_stream` (tests.rs:1947)
- **Related Solution:** [nats-session-log-mutations-canonical-resolution-2026-06-23.md](nats-session-log-mutations-canonical-resolution-2026-06-23.md)
- **Related Solution:** [stacked-mutation-replay-rposition-2026-05-01.md](stacked-mutation-replay-rposition-2026-05-01.md) — rposition for stacked edits
