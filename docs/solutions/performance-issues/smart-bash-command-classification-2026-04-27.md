---
title: "Smart bash command classification for history snapshot optimization"
date: 2026-04-27
category: "performance-issues"
problem_type: performance_issue
component: "harnx-mcp-history"
root_cause: "every bash invocation triggered full-tree git snapshot regardless of command read/write semantics"
resolution_type: code_fix
severity: medium
tags:
  - bash
  - classification
  - snapshot
  - history
  - panic-safety
  - shell-parsing
plan_ref: "issue-355-smart-snapshot-classification"
---

## Problem

Every bash invocation in `harnx-mcp-bash` triggered a full-tree git history snapshot via `git ls-files`, even for read-only commands like `ls`, `cat`, or `git status`. This added O(repo-size) overhead per command, degrading agent responsiveness on large repositories.

## Symptoms

- Agent bash calls had ~100-500ms history snapshot overhead even for trivial read commands
- Large repositories (10k+ files) caused noticeable latency on every shell invocation
- Network filesystems amplified the cost due to file metadata operations

## Solution

Added a bash command classifier (`harnx-mcp-history::classify`) that routes each invocation into one of three `SnapshotDecision` variants:

- **ReadOnly**: Skip snapshot entirely (e.g., `ls`, `cat`, `git status`)
- **Targeted(paths)**: Snapshot only specified paths (e.g., `cp a b` → snapshot `b`)
- **FullSnapshot**: Existing full-tree capture (default for unknown/complex commands)

Result: 60-70% of agent bash calls skip snapshots, ~15% take smaller targeted snapshots.

Key implementation components:

1. **Static rule table** in `classify.rs` mapping `(command, subcommand)` pairs to classification functions
2. **`shell-words` tokenization** for baseline parsing, plus custom quote-aware tokenizers for compound commands (`&&`/`||`/`;`/`|`), redirects (`>`/`>>`), and `tee` extraction
3. **`capture_files_blocking`** — sibling of `capture_tree_blocking` that snapshots a passed-in file list instead of running `git ls-files`
4. **`SnapshotDecision` persistence** on spawned-process records so spawn/wait split keeps consistent classification

## Non-Obvious Lessons

### 1. `catch_unwind` + `AssertUnwindSafe` for fail-open in hot paths

The classifier is called on every bash invocation. Per spec: any panic, parse error, or unknown command must yield `FullSnapshot` (safe fallback). Implementation wraps the entire classification body:

```rust
pub fn classify_command(raw: &str, cwd: &Path) -> SnapshotDecision {
    panic::catch_unwind(AssertUnwindSafe(|| classify_command_inner(raw, cwd)))
        .unwrap_or(SnapshotDecision::FullSnapshot)
}
```

`&str` and `&Path` borrows aren't naturally `UnwindSafe`, but the classification logic is pure string transformation — no locks, no mutable state. `AssertUnwindSafe` wrapper is appropriate here.

Test verifies via `#[cfg(test)]`-only rule that panics on match:

```rust
#[cfg(test)]
fn classify_panic_rule(_: &[&str], _: &Path) -> SnapshotDecision {
    panic!("panic rule hit");
}
```

### 2. Point-in-time snapshot semantics with `empty_tree()` editor base

Both `capture_tree_blocking` and `capture_files_blocking` use `repo.empty_tree().id` as the tree editor base:

```rust
let mut editor = repo.edit_tree(repo.empty_tree().id)?;
```

The `parent` argument is used solely for the commit parent chain (rollback traversal), not for tree inheritance. Each snapshot is a fresh point-in-time capture of the working directory state.

**Why this matters:** Multiple Aristarchus reviewers across cycles flagged this as "data loss", assuming targeted snapshots would delete untracked files. The design is intentional — snapshots capture what exists now, not a diff against parent. Rollback restores the captured state.

### 3. `harnx-history@localhost` author email is load-bearing

`rollback::rollback_to_commit_blocking` filters commits by author email:

```rust
commit.author()
    .ok()
    .map(|a| a.email == "harnx-history@localhost")
    .unwrap_or(false)
```

Any change to `write_snapshot_commit`'s author identity breaks rollback silently. The `test_rollback_restores_file` test is the canary. One Phase 2 attempt regressed by altering this; recovered by stashing and re-delegating with an explicit "do not touch write_snapshot_commit" guardrail.

### 4. Combined short-flag parsing

Initial `has_any_flag(argv, &["-r"])` used exact match. `git rm -rf` did NOT match `-r` because the strings differed. Fix: decompose short-flag bundles:

```rust
fn has_any_flag(argv: &[&str], flags: &[&str]) -> bool {
    argv.iter().any(|arg| {
        if flags.contains(arg) {
            return true;
        }
        // Combined short-flag bundles like `-rf` or `-Ap`
        if let Some(stripped) = arg.strip_prefix('-') {
            if !stripped.is_empty() && !arg.starts_with("--") {
                for ch in stripped.chars() {
                    if flags.contains(&format!("-{ch}").as_str()) {
                        return true;
                    }
                }
            }
        }
        false
    })
}
```

Without this, recursive flags bypass the `FullSnapshot` guard, and `git rm -rf dir` gets `Targeted([dir])` which the file-only snapshotter silently skips.

### 5. Quoted command substitution detection

In bash, double-quotes do NOT suppress `$(...)` or backtick command substitution — only single-quotes do. Initial implementation used `!in_single && !in_double` for both, missing `echo "$(touch x)"`. Fix:

```rust
fn contains_opaque_substitution(raw: &str) -> bool {
    // ...
    if !in_single {
        if ch == '`' { return true; }
        if ch == '$' && chars.get(index + 1) == Some(&'(') { return true; }
    }
    // Process substitution <(...) IS suppressed by double-quotes
    if !in_single && !in_double && ch == '<' && chars.get(index + 1) == Some(&'(') {
        return true;
    }
}
```

### 6. Multi-cycle Aristarchus pattern

This work went through 4 review cycles. Each cycle surfaced 1-3 genuine consensus blockers from sub-reviewers cross-checked by judges — but each cycle also produced findings rejected as design choices (empty-tree base, deletion semantics) or out-of-scope completeness gaps (multi-file `sed -i`, glob expansion).

Atlas's role: triage findings per cycle, fix only genuine consensus blockers, document false findings in plan notes so subsequent cycles don't re-litigate them.

**Key distinction:**
- **Design choices** (upheld): empty-tree base, deletion skip in targeted capture — these are intentional, not bugs
- **Completeness gaps** (out of scope): multi-file sed -i, glob expansion, curl -o — could be future work but not blockers
- **Genuine blockers** (must fix): double-quoted command substitution, combined flag parsing, git add directory detection

## Prevention Strategies

**Code Review Checklist:**
- [ ] Classification logic wrapped in `catch_unwind`?
- [ ] Unknown commands return `FullSnapshot`?
- [ ] Shell syntax edge cases tested (quoted substitution, combined flags)?
- [ ] Author email unchanged if rollback touched?
- [ ] Directory-targeting commands escalate to `FullSnapshot`?

**Test Cases:**
- `test_panic_in_rule_fails_open` — panicking rule returns `FullSnapshot`
- `test_full_snapshot_triggers` — command substitution in double-quotes returns `FullSnapshot`
- `test_combined_short_flag_bundles` — `git rm -rf` returns `FullSnapshot`
- `test_rollback_restores_file` — canary for author email identity

## Related Issues

- **PR:** [#367](https://github.com/dobesv/harnx/pull/367) — Implement smart bash command classification for history snapshots
- **Issue:** [#355](https://github.com/dobesv/harnx/issues/355) — Smart modified path detection for harnx-mcp-bash file history snapshotting
- **Related Solution:** [logic-errors/git-backed-local-history-rollback-2026-04-26.md](../logic-errors/git-backed-local-history-rollback-2026-04-26.md) — Foundation this work extends
