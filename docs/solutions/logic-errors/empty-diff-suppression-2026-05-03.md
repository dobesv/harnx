---
title: "Empty diff suppression in markdown-fenced output"
date: 2026-05-03
category: "logic-errors"
problem_type: logic_error
component: "harnx-mcp-history"
root_cause: "wrapper-always-emits-non-empty"
resolution_type: code_fix
severity: medium
tags:
  - diff
  - markdown
  - is_empty-guard
  - output-suppression
plan_ref: "issue-444-blank-diffs"
---

## Problem

`diff_commits_blocking` wrapped git diff output in markdown fences even when the diff body was empty. This produced `header + "```diff\n\n```\n"` — a non-empty string that defeated the caller's `!diff.is_empty()` guard, causing blank diff blocks to appear in transcripts after every command in git-tracked directories.

## Symptoms

- Blank ` ```diff ``` ` blocks in transcript after every bash/fs command execution
- Occurred only in git-tracked directories with no file changes between snapshots
- Guard `Ok(diff) if !diff.is_empty()` in caller failed to suppress output
- Visual noise: transcript cluttered with empty fenced code blocks

## Investigation Steps

1. Observed blank diff blocks appearing in transcript after commands in git-tracked directories
2. Traced output to `diff_commits_blocking` in `harnx-mcp-history/src/diff.rs`
3. Found function always returned `format!("{header}```diff\n{body}\n```\n")` regardless of `body` content
4. When git diff produces no output (no changes), `body` is empty but the formatted string is still ~50+ characters
5. Caller's `!diff.is_empty()` guard cannot detect "no meaningful output"
6. Root cause: wrapper always emits non-empty string even with empty content

## Root Cause

The function built the complete markdown-wrapped output (header + fence + body + fence) before checking if there was anything to wrap. The `is_empty()` guard in callers checks the *wrapped* result, not the *unwrapped* content. When `body` is empty but the function still returns `header + "```diff\n\n```\n"`, the guard fails.

**Key insight:** When a function always produces wrapping (header, fence, markup) regardless of content emptiness, callers cannot reliably detect "no meaningful output" with a simple `is_empty()` check on the result.

## Solution

Added early return of `Ok(String::new())` immediately after UTF-8 conversion of git diff output, before building the header+fence:

```rust
let mut body = String::from_utf8(output.stdout).context("git diff output not utf-8")?;

// Nothing changed between the two snapshots — don't emit any output.
// The caller already guards on `!diff.is_empty()`, so returning an
// empty string suppresses the blank ```diff\n``` fence that would
// otherwise appear in the transcript (issue #444).
if body.is_empty() {
    return Ok(String::new());
}

// ... rest of function builds header + fence
```

**Test added:** `test_diff_commits_no_changes_returns_empty` verifies:
1. Same-commit-to-itself diff returns empty string
2. Two commits with unrelated file changes still produces non-empty diff

## Why This Works

The early return checks the *content* (`body`) before wrapping, not the *result* after wrapping. By returning an empty string when there's nothing to show, the existing caller guard `!diff.is_empty()` correctly suppresses output. The function signature remains unchanged — it still returns `Result<String>`, just sometimes an empty one.

The test ensures we only suppress when the diff body itself is empty (git diff exits 0 with no output), not when it contains changes to unrelated files.

## Prevention Strategies

**Design Pattern:**
- When a function wraps output (adds header/footer/fence/markup), check if the *unwrapped content* is empty before wrapping
- Return empty/sentinel value for empty content, not empty-wrapped output
- Let callers decide what to render based on return value

**Code Review Checklist:**
- [ ] Does this function wrap output in headers/fences/markup?
- [ ] Is there an early return for empty content *before* wrapping?
- [ ] Will caller's guards work correctly on the wrapped result?

**Test Coverage:**
- Test empty content returns empty/nil (not wrapped-empty)
- Test that non-empty content still gets wrapped correctly
- Test edge cases (whitespace-only, single character, etc.)

## Related Issues

- **GitHub:** [#444](https://github.com/example/harnx/issues/444) — Transcript showing blank diffs after every command
- **File:** `crates/harnx-mcp-history/src/diff.rs`
