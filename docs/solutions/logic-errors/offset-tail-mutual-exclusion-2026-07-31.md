---
title: "Removed offset+tail mutual exclusion in read tools, fixed off-by-one validation bug"
date: 2026-07-31
category: "logic-errors"
problem_type: logic_error
component: "harnx-fs-tools, harnx-mcp-bash"
root_cause: "unnecessary mutual exclusion between parameters; duplicated validation with inconsistent boundary check"
resolution_type: code_fix
severity: medium
tags:
  - api-ergonomics
  - off-by-one
  - parameter-validation
  - pagination
plan_ref: "harnx-1312-offset-tail-combine"
---

## Problem

The `read` tool (harnx-fs-tools) and `read_exec_log` tool (harnx-mcp-bash) rejected `offset` + `tail` together with error "offset and tail are mutually exclusive". Agents hit this frequently. No fundamental conflict exists — the parameters compose naturally.

## Symptoms

- Error: `offset and tail are mutually exclusive` when both parameters provided
- Workaround: agents manually combined semantics by reading entire file then post-processing
- Friction in pagination flows where skip-to-line + tail-from-end is a natural operation

## Investigation Steps

Reviewed the validation logic in both crates:
- `handlers.rs` in harnx-fs-tools (`read_file_impl`) had a guard returning error if both `offset` and `tail` were set
- `exec_log.rs` in harnx-mcp-bash (`validate_read_exec_log_params`) had an identical guard
- Neither tool had a semantic reason for the exclusion — the combination is well-defined

Traced through the implementation path to design the combined semantics:
1. Skip to line `offset` (1-indexed, so skip `offset - 1` lines)
2. On the remaining window (lines `offset..end`), return the last `tail` lines

## Root Cause

Two issues:

1. **Unnecessary mutual exclusion**: The original implementation treated `offset` and `tail` as competing ways to define a read window, but they actually compose: offset defines the window start, tail defines how much of that window to return from the end.

2. **Off-by-one validation bug**: When implementing the tail branch, the check used `skip > total` instead of `offset > total`. This let `offset = total+1` slip through and return `Ok(empty)` instead of erroring — inconsistent with the non-tail branch which correctly rejected `offset > total`.

## Solution

Removed the mutual exclusion guard and unified the calculation:

**Shared math (both crates):**
```
skip = offset - 1
window_len = total - skip
start = max(total - tail, skip)
return lines[start..]
```

**Boundary validation:** hoisted to run once before branching on tail:
```rust
if offset > total {
    return Err("offset beyond end of file");
}
```

Both paths now share identical offset validation semantics.

**Files changed:**
- `crates/harnx-fs-tools/src/server/handlers.rs` — removed the mutual-exclusion guard in `read_file_impl`; combined offset+tail logic in `paginate_read_lines`
- `crates/harnx-fs-tools/src/server/params.rs` — updated the `tail` schema description
- `crates/harnx-fs-tools/src/server/tests.rs` — new test cases for combined params
- `crates/harnx-mcp-bash/src/server/exec_log.rs` — removed the guard in `validate_read_exec_log_params`; combined offset+tail logic in `select_log_lines`
- `crates/harnx-mcp-bash/src/server/params.rs` — updated the `tail` schema description
- `crates/harnx-mcp-bash/src/server/tests.rs` — new test cases

**Additional fixes:**
- "Showing last N of M matching lines" notice reports `window_len` (post-offset window), not total
- When `tail` is set, suppress the forward "more matching lines" pagination notice — tail anchors to end, nothing follows

## Why This Works

The parameters express orthogonal concepts:
- `offset`: where to start reading (absolute position from file start)
- `tail`: how much to return from the window's end (relative to the window defined by offset)

For a 100-line file with `offset=20, tail=10`:
- Window becomes lines 20..100 (81 lines)
- Return last 10 of that window: lines 91..100

The validation bug was caught because the duplicated check used different boundary logic (`skip > total` vs `offset > total`). Since `skip = offset - 1`, `skip > total` is equivalent to `offset > total + 1`, allowing the off-by-one slip.

## Prevention Strategies

**When duplicating a check across branches, validate BEFORE branching:**

```rust
// BAD: duplicated check with subtle difference
if tail.is_some() {
    if skip > total { return Err(...); }  // off-by-one!
    // tail path
} else {
    if offset > total { return Err(...); }
    // non-tail path
}

// GOOD: validate once, then branch
if offset > total { return Err(...); }
if tail.is_some() {
    // tail path
} else {
    // non-tail path
}
```

**Test boundary cases explicitly:**
- `offset = total` (last line) — should succeed
- `offset = total + 1` — should error, not return empty
- `offset + tail` where tail exceeds window — return entire window

**Code review checklist:**
- [ ] When adding a branch, check if validation is duplicated
- [ ] Hoist shared validation above the branch point
- [ ] Compare boundary conditions letter-for-letter between branches
