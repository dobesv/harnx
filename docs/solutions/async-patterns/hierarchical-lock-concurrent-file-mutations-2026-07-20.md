---
title: "Hierarchical two-level locking for concurrent file mutations in MCP filesystem server"
date: 2026-07-20
category: async-patterns
problem_type: logic_error
component: harnx-mcp-fs
root_cause: "non-atomic read-modify-write with no serialization across concurrent tool calls"
resolution_type: code_fix
severity: high
tags:
  - tokio
  - concurrency
  - locking
  - mcp-server
  - deadlock-prevention
  - rwlock
  - weak-references
plan_ref: issue-1101-fs-edit-serialization
---

## Problem

Filesystem MCP server's mutating tools (`write`, `edit`, `insert`, `re_replace`, `rollback_file`) each performed non-atomic read-modify-write plus git history snapshots without locking. On a multi-threaded tokio runtime, concurrent `call_tool` invocations interleaved → file corruption, lost updates, truncated content. GitHub issue #1101.

## Symptoms

- Parallel edits to same file caused content corruption (truncation, mangling)
- Lost updates when multiple handlers read-modify-wrote concurrently
- `rollback_file` could interleave with edits to other files in same repo, corrupting working tree
- Only manifested under actual concurrent agent tool batches; not reliably reproducible in sequential tests

## Investigation Steps

1. Identified all mutating handlers shared common pattern: validate → read file → modify → write → git snapshot
2. Traced handler invocations through `rmcp` service layer → each `call_tool` runs concurrently on tokio runtime
3. Recognized `rollback_file` as special case: mutates entire repo working tree, not just single file
4. Initial per-file lock approach failed review: rollback keyed on `path` argument, but concurrent edits to OTHER files in same repo took different locks → could interleave
5. Designed hierarchical two-level lock: repo-level RwLock (shared for file edits, exclusive for rollback) + per-file Mutex

## Root Cause

`FsServer` was Clone with shared state behind Arc, but no synchronization primitives. Each concurrent tool handler ran independently. The read-modify-write sequence is not atomic, so interleaving produced conflicts. `rollback_file` was especially problematic because it discovers repo root internally and mutates the whole working tree, but had no way to exclude concurrent file edits in the same repo.

## Solution

Added two lock registries to `FsServer`:

```rust
pub struct FsServer {
    // ...existing fields...
    repo_locks: Arc<std::sync::Mutex<HashMap<PathBuf, Weak<tokio::sync::RwLock<()>>>>>,
    file_locks: Arc<std::sync::Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>>,
}
```

### Lock Registry Helpers

```rust
fn repo_lock_key_for_path(path: &Path) -> PathBuf {
    harnx_mcp_history::discover::find_repo_for_path(path)
        .or_else(|| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
}

fn repo_lock_for_path(&self, path: &Path) -> Arc<RwLock<()>> {
    let key = Self::repo_lock_key_for_path(path);
    let mut locks = self.repo_locks.lock().expect("...");
    locks.retain(|_, weak| weak.strong_count() > 0);  // prune dead entries
    
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    
    let lock = Arc::new(RwLock::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn lock_for_path(&self, path: &Path) -> Arc<AsyncMutex<()>> {
    // Same pattern for per-file locks
}
```

### Lock Protocol (Uniform Order → Deadlock-Free)

**File mutators** (write/insert/re_replace/edit):
1. Acquire repo `RwLock` in READ (shared) mode
2. Acquire per-file `Mutex` (exclusive)
3. Hold both guards for entire handler via RAII

**Rollback** (repo-wide operation):
1. Validate repo root BEFORE taking lock (fail fast)
2. Acquire repo `RwLock` in WRITE (exclusive) mode only
3. No per-file lock needed — write lock excludes all file editors

```rust
// File mutator pattern
let repo_lock = self.repo_lock_for_path(&path);
let file_lock = self.lock_for_path(&path);
let _repo_guard = repo_lock.read().await;
let _file_guard = file_lock.lock().await;
// ... perform mutation ...

// Rollback pattern
let repo_lock = self.repo_lock_for_path(&repo_dir);
let _repo_guard = repo_lock.write().await;
// ... rollback entire working tree ...
```

### Semantics

- Different files, same repo → concurrent (each holds repo READ + different file Mutex)
- Same file → serialized (same file Mutex)
- Rollback vs any edit in same repo → mutually exclusive (repo WRITE blocks all READs)

## Why This Works

1. **Hierarchical ordering ensures deadlock prevention**: All handlers acquire repo lock first, then file lock. Rollback skips file lock. No cycles possible.

2. **Weak-valued registry with retain prevents unbounded growth**: Each lookup prunes dead entries. No background task needed. Locks freed when last Arc dropped.

3. **std::sync::Mutex guards never held across .await**: Registry mutex protects only the HashMap lookup/insert synchronously. Async locks acquired after releasing std guard.

4. **Repo lock keyed on discovered root**: All path variants within same repo map to same repo lock. Uses `find_repo_for_path` to resolve canonical repo root.

5. **Fail-fast before exclusive hold**: Rollback validates repo existence BEFORE acquiring write lock, minimizing time spent with writers blocked.

## Prevention Strategies

**Test Cases:**
- Stress test with 32 concurrent inserts to one file → all survive (no lost updates)
- Verify rollback excludes concurrent edits to different file in same repo
- Verify same-repo paths share one repo lock; different repos get different locks
- Run under `cargo nextest run -p harnx-mcp-fs --stress-count=5`

**Code Review Checklist:**
- [ ] Is lock acquisition order consistent across ALL handlers?
- [ ] Are std::sync::Mutex guards NEVER held across .await points?
- [ ] Does repo-wide operation acquire repo write lock before any mutation?
- [ ] Is validation performed BEFORE taking exclusive locks (fail fast)?

**Best Practices:**
- Use hierarchical locks (coarse + fine-grained) to allow parallelism where possible
- Key locks on canonical/discovered values, not user-provided paths
- Prune weak-valued registries on lookup to prevent memory leaks
- Document accepted edge cases and residual risks explicitly

## Known Limitations

1. **File creation path aliasing**: For non-existent write targets, `validate_write_path` returns non-canonical path. Path aliases (case differences) can key different file locks, bypassing same-file serialization for *creation*. Accepted as low-risk edge case.

2. **Read-only tools don't lock**: Can observe transient state during rollback. Acceptable for current use case; read consistency would require repo read lock on all reads.

3. **Poison propagation**: If a handler panics holding a lock, subsequent acquisitions unwrap poison. Could use `into_inner()` for recovery if needed.

## Related Issues

- **GitHub:** [#1101](https://github.com/dobesv/harnx/issues/1101) — Original issue
- **Changeset:** `.changeset/fs-server-per-file-locking.md`
- **Related:** [session-actor-concurrency-invariants-2026-07-04.md](../async-patterns/session-actor-concurrency-invariants-2026-07-04.md) — Actor concurrency patterns
- **Related:** [mcp-server-background-task-supervision-2026-05-25.md](../async-patterns/mcp-server-background-task-supervision-2026-05-25.md) — spawn_blocking for fs I/O
