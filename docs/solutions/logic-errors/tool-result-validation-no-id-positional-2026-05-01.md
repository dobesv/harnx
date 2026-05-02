---
title: "Tool result validation supports all-no-ID positional matching"
date: 2026-05-01
category: "logic-errors"
problem_type: logic_error
component: "harnx-session-log"
root_cause: "validation rejected missing tool_call_id without considering legitimate all-absent case for positional matching"
resolution_type: code_fix
severity: medium
tags:
  - session-log
  - tool-pair-validation
  - positional-matching
  - serde
  - field-pattern-trap
plan_ref: "harnx-342-phase2"
---

## Problem

`.edit` command on entries containing tool calls/results failed validation when tool results lacked `tool_call_id` fields, even though some LLM providers never include these IDs. Validation needed to support positional matching when ALL results lack IDs.

## Symptoms

- `.edit message 5` on tool-call entries failed with "missing tool_call_id" error
- Users with OpenAI-compatible providers that omit `tool_call_id` in results could not edit sessions
- `validate_tool_pair_integrity` tests failed for no-ID scenarios
- Existing sessions with no-ID tool results could not be edited

## Investigation Steps

1. Reviewed `validate_tool_pair_integrity` function in `mod.rs`
2. Found the validation loop checking each result for `tool_call_id`
3. Realized the validation only handled the "all IDs present" case
4. Consulted API docs — some providers return results without IDs, relying on positional correspondence
5. Identified three valid scenarios:
   - All results have IDs → validate by ID matching
   - All results lack IDs → validate by count matching (positional)
   - Mixed (some with, some without) → invalid, reject explicitly

## Root Cause

The validation function iterated over results and rejected any missing `tool_call_id`. This was too strict.

Some LLM providers (especially non-OpenAI) return tool results without `tool_call_id` fields. The LLM is expected to match results to calls by position: first result goes with first call, second with second, etc.

The validation needed to:
1. Check if ALL results lack IDs → positional matching (count must match)
2. Check if MIXED presence → error (ambiguous)
3. Check if ALL results have IDs → ID matching (IDs must match call IDs)

## Solution

Updated `validate_tool_pair_integrity` in `crates/harnx-runtime/src/config/mod.rs`:

**Before:**
```rust
for result in results {
    let call_id = result.id.as_deref().filter(|id| !id.is_empty());
    if call_id.is_none() {
        bail!("tool result missing tool_call_id");
    }
    if !call_ids.contains(call_id.unwrap()) {
        bail!("unknown tool_call_id");
    }
}
```

**After:**
```rust
let missing_result_ids = results
    .iter()
    .filter(|result| result.id.as_deref().is_none_or(str::is_empty))
    .count();

// All results lack IDs → positional matching by count
if missing_result_ids == results.len() {
    if results.len() != calls.len() {
        bail!(
            "Edited tool result at {result_seq} is missing tool_call_id for positional matching and count {} does not match tool calls count {}",
            results.len(), calls.len()
        );
    }
    continue;  // positional match OK
}

// Mixed presence → ambiguous, reject
if missing_result_ids > 0 {
    bail!(
        "Edited tool result at {result_seq} mixes tool_call_id values with missing tool_call_id entries"
    );
}

// All results have IDs → validate by ID matching
for result in results {
    let call_id = result.id.as_deref().filter(|id| !id.is_empty()).unwrap();
    if !call_ids.contains(call_id) {
        bail!("unknown tool_call_id '{}'", call_id);
    }
}
```

## Why This Works

The validation now handles all three cases explicitly:

1. **All-no-ID (positional)**: When every result lacks `tool_call_id`, the LLM provider is using positional matching. We validate that the count matches (3 calls = 3 results).

2. **Mixed presence**: This is ambiguous. We can't tell if result[0] without ID corresponds to call[0] while result[1] with ID "call-2" corresponds to... which call? Safer to reject.

3. **All-have-ID (ID-based)**: Standard OpenAI-style matching. Each result's ID must match one of the call IDs.

The count check ensures positional validity: if an edit produces 2 results for 3 calls, the LLM will fail to match them at inference time anyway. Better to catch this at edit validation.

## Prevention Strategies

**Test cases:**
```rust
#[test]
fn validate_tool_pair_integrity_accepts_positional_tool_results_without_ids() {
    let documents = vec![
        tool_calls_yaml(&["call-1", "call-2"]),
        tool_results_yaml_with_optional_ids(&[None, None]),
    ];
    validate_tool_pair_integrity(4, &documents).unwrap();
}

#[test]
fn validate_tool_pair_integrity_rejects_mixed_present_and_missing_result_ids() {
    let documents = vec![
        tool_calls_yaml(&["call-1", "call-2"]),
        tool_results_yaml_with_optional_ids(&[Some("call-1".to_string()), None]),
    ];
    let err = validate_tool_pair_integrity(12, &documents).expect_err("mixed id presence should fail");
    assert!(err.to_string().contains("mixes tool_call_id values with missing"));
}
```

**Best practices:**
- When validating optional fields, consider whether "all present" vs "all absent" vs "mixed" have different semantics
- Document the three-case logic explicitly in comments

**Code review checklist:**
- [ ] Does validation handle optional field correctly in all three cases?
- [ ] Are there tests for the all-absent case?
- [ ] Are there tests for the mixed case?

## Related Issues

- **PR:** #423 — Non-destructive session history editing
- **Related Solution:** [non-destructive-session-mutation-two-pass-replay-2026-05-01.md](non-destructive-session-mutation-two-pass-replay-2026-05-01.md)
