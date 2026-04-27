---
title: "Git-backed local history and non-destructive rollback for MCP servers"
date: 2026-04-26
category: "logic-errors"
problem_type: logic_error
component: "harnx-mcp-history"
root_cause: "design constraint — async server with non-Sync git library needed thread-safe patterns"
resolution_type: code_fix
severity: medium
tags:
  - gix
  - async
  - git
  - rollback
  - thread-safety
  - spawn_blocking
plan_ref: "harnx-318-local-history"
---

## Problem

MCP servers (fs, bash) needed git-backed local history with rollback capability, but gix's `Repository` is `!Sync`, making it unusable directly in async contexts. Rollback via `git checkout` is destructive. Needed non-destructive, fully reversible rollback with atomicity guarantees.

## Symptoms

- `gix::Repository` cannot be held across await points in async functions
- `git checkout <sha> -- <path>` destructively overwrites working tree, cannot itself be undone
- Naive `repo.reference("HEAD", oid, ...)` on an attached-HEAD repo detaches HEAD
- `git ls-files` output with `.lines()` fails on filenames containing newlines

## Investigation Steps

Started with gix 0.82.0 for pure-Rust git operations. Needed to store repository handles in async server state. `gix::Repository` is `!Sync` (not thread-safe). Initial rollback design used `git checkout` — rejected as destructive.

Explored forward-undo approach: apply target tree state via fs I/O, then commit the result. Required careful handling of HEAD ref updates to avoid detaching attached branches.

File collection with `git ls-files` needed NUL-delimited output to handle edge-case filenames.

Session ref namespace `refs/harnx-history/<uuid>` chosen for:
1. Survives `git gc` (real refs, not dangling objects)
2. Scoped per session (multiple sessions don't collide)
3. Provenance validation via prefix scan in `is_harnx_history_commit`

## Root Cause

**gix `!Sync` constraint:** `gix::Repository` contains non-thread-safe internal state. Cannot be stored in structs shared across threads (e.g., inside `Arc<Mutex<...>>` used by async servers).

**Destructive rollback:** `git checkout` mutates working tree without creating a commit. Cannot be undone by git.

**HEAD detach trap:** `repo.reference("HEAD", oid, ...)` creates a direct ref to the OID, detaching HEAD from its branch. Must advance the branch ref directly instead.

**NUL-split requirement:** `.lines()` splits on `\n`, but filenames can contain newlines. `git ls-files -z` uses NUL (`\0`) as delimiter.

## Solution

### 1. ThreadSafeRepository + spawn_blocking Pattern

Store `gix::ThreadSafeRepository` (created via `repo.into_sync()`), then convert to thread-local inside `spawn_blocking`:

```rust
struct RepoSession {
    repo: gix::ThreadSafeRepository,
    last_commit_id: Option<gix::ObjectId>,
    session_ref: String,
}

impl HistoryManager {
    pub async fn snapshot(&self, paths: &[PathBuf], label: &str) -> Result<gix::ObjectId> {
        let (repo_workdir, ts_repo, parent, session_ref) = {
            let repos = self.inner.repos.lock().await;
            // ... extract from mutex ...
        };

        let commit_id = tokio::task::spawn_blocking(move || {
            let repo = ts_repo.to_thread_local();  // Thread-local borrow
            capture_tree_blocking(&repo, &repo_workdir, parent, &session_ref, &label)
        })
        .await
        .context("snapshot task join failed")??;

        Ok(commit_id)
    }
}
```

All gix I/O happens inside `spawn_blocking`, outside async context.

### 2. Before/After Snapshot Pattern for Atomicity

Every mutating operation follows: before-snapshot → operation → after-snapshot

```rust
// In rollback_to_commit_blocking
let before_id = capture_tree_blocking(repo, workdir, parent, session_ref, "before rollback")?;

// ... apply file mutations via fs I/O ...

let after_id = capture_tree_blocking(repo, workdir, Some(before_id), session_ref, &after_label)?;
```

If operation fails midway, before-snapshot already captured all files in git. Nothing lost.

### 3. Forward-Undo Rollback (No git checkout)

Rollback applies target tree state via fs operations, then commits:

1. Flatten both target tree and HEAD tree to path→blob_id maps
2. For each path in union: write/delete file to match target state
3. After-snapshot creates new commit on HEAD
4. Advance branch ref (or HEAD if detached) to after-snapshot

```rust
let target_files = flatten_tree(repo, target_tree_id)?;
let head_files = flatten_tree(repo, head_tree.id().detach())?;

for relative_path in all_paths {
    match (target_files.get(&relative_path), head_files.get(&relative_path)) {
        (Some(target_blob), _) => {
            let data = repo.find_object(*target_blob)?.data.to_owned();
            fs::write(&full_path, data)?;
        }
        (None, Some(_)) => {
            fs::remove_file(&full_path)?;
        }
        _ => {}
    }
}
```

Result: working tree at target state, but as a new commit that can be reverted.

### 4. HEAD Ref Update Without Detaching

Check `head.referent_name()` to distinguish attached vs detached HEAD:

```rust
let head = repo.head().context("read HEAD for rollback ref update")?;
if let Some(branch) = head.referent_name() {
    // Attached HEAD — advance the branch ref directly
    // Pass branch as &FullNameRef (not String)
    repo.reference(
        branch,  // &FullNameRef
        after_id,
        gix::refs::transaction::PreviousValue::Any,
        "harnx rollback",
    )?;
} else {
    // Detached HEAD — update HEAD directly
    repo.reference("HEAD", after_id, gix::refs::transaction::PreviousValue::Any, "harnx rollback")?;
}
```

### 5. NUL-Delimited git ls-files

```rust
let output = std::process::Command::new("git")
    .args(["-z", "ls-files", "--cached", "--others", "--exclude-standard"])
    .current_dir(root)
    .output()?;

let files: Vec<PathBuf> = output
    .stdout
    .split(|&b| b == 0)  // NUL-delimited, not .lines()
    .filter(|s| !s.is_empty())
    .filter_map(|s| std::str::from_utf8(s).ok())
    .map(|line| root.join(line))
    .filter(|p| p.is_file())
    .collect();
```

### 6. Session Refs in refs/harnx-history/<uuid>

```rust
let session_id = Uuid::new_v4().to_string();
let session_ref = format!("refs/harnx-history/{session_id}");
```

Provenance validation only scans this prefix:

```rust
fn is_harnx_history_commit(repo: &gix::Repository, commit_id: gix::ObjectId) -> bool {
    let prefix = "refs/harnx-history/";
    repo.references()
        .ok()
        .and_then(|refs| {
            refs.prefixed(prefix.as_bytes()).ok().map(|iter| {
                iter.flatten().any(|r| r.target().try_id() == Some(commit_id.as_ref()))
            })
        })
        .unwrap_or(false)
}
```

## Why This Works

- **ThreadSafeRepository** wraps repository in thread-safe container. `to_thread_local()` creates thread-local borrow valid within `spawn_blocking` scope. No `!Sync` violations.
- **Before/after snapshots** ensure atomicity: if operation fails, pre-op state preserved in git history. After-snapshot captures post-op state for diff/rollback.
- **Forward-undo rollback** creates new commits rather than mutating working tree. Fully reversible via `git revert`.
- **Branch ref advancement** preserves HEAD attachment. `referent_name()` returns branch name as `FullNameRef` which gix accepts directly.
- **NUL-delimited output** handles all valid filenames including those with newlines.
- **Session ref namespace** provides scoped, garbage-collected refs with efficient prefix-based provenance checks.

## Prevention Strategies

**Code Review Checklist:**
- [ ] All gix operations inside `spawn_blocking`?
- [ ] ThreadSafeRepository stored, Repository only borrowed thread-local?
- [ ] mutating operations use before/after snapshot pattern?
- [ ] HEAD ref updates check `referent_name()` before calling `reference()`?
- [ ] File Listing uses NUL-delimited parsing?
- [ ] Session refs under `refs/harnx-history/` prefix?

**Test Cases:**
- Rollback with attached HEAD preserves branch attachment
- Rollback with detached HEAD remains detached
- Before-snapshot preserved even if rollback fails mid-operation
- Filenames with newlines handled correctly
- `is_harnx_history_commit` only matches commits in harnx-history refs

## Related Issues

- **PR:** [#318](https://github.com/example/harnx/pull/318) — feat(mcp): add git-backed local history and rollback
- **Crate:** `harnx-mcp-history` — new crate providing HistoryManager, snapshot, diff, and rollback functionality
