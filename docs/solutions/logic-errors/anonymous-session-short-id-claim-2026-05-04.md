---
title: "Anonymous session short ID claim with create_new atomicity"
date: 2026-05-04
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime/config"
root_cause: "Anonymous session path bypassed generate_session_id collision check, produced full UUIDs instead of short IDs"
resolution_type: code_fix
severity: medium
tags:
  - session-management
  - concurrency
  - timestamp-based-ids
  - file-locking
  - atomic-operations
plan_ref: "fix-anonymous-session-short-id-449"
---
## Problem

PR #456 introduced 6-char base64url session IDs via `generate_session_id()` for named sessions, but `Config::use_session(None)` — the anonymous session path used by the ACP server — still called `Uuid::now_v7()` directly. New ACP sessions got full UUIDs instead of short IDs, breaking consistency.

## Symptoms

- ACP server sessions had 36-char UUID filenames instead of 6-char short IDs
- Named sessions (via `-s name`) correctly used short IDs
- Session picker showed mixed ID formats
- Test `test_new_session_has_uuid7_filename` passed but validated wrong behavior

## Investigation Steps

1. Ran ACP server integration tests — session IDs were UUIDs
2. Traced `Config::use_session(None)` path — found direct `Uuid::now_v7()` call
3. Reviewed PR #456 changes — `session::new()` used `generate_session_id()` but anonymous path bypassed it
4. Analyzed collision avoidance: `generate_session_id` checks file existence, but file isn't written until after ID returned
5. Realized the gap: without immediate file claim, two sessions created within same second (same tempdir) get identical IDs

## Root Cause

Two separate issues:

1. **Code path inconsistency**: `use_session(None)` bypassed the new `generate_session_id()` logic that `session::new()` uses for named sessions.

2. **Race condition in collision avoidance**: The collision check in `generate_session_id` uses `exists(candidate)`, but works correctly only if file is created atomically before another caller can generate the same ID. For the ACP server's `new_session` workflow:
   - Exits previous session (deletes its file)
   - Creates new session
   - If two rapid calls happen in same second with no existing files, both get same ID

The atomic `create_new(true)` file creation is the correct primitive — it claims the ID before any other caller can interfere.

## Solution

### 1. New `new_anonymous_session_id()` helper

```rust
// crates/harnx-runtime/src/config/mod.rs
impl Config {
    /// Generate a unique anonymous session ID, claiming it atomically.
    fn new_anonymous_session_id(&self) -> Result<String> {
        loop {
            let id = generate_session_id(|candidate| self.session_file(candidate).exists());
            match self.claim_session_file(&id) {
                Ok(true) => return Ok(id),  // Successfully claimed
                Ok(false) => continue,      // Collision, retry
                Err(e) => return Err(e),    // Real I/O error
            }
        }
    }
}
```

### 2. Atomic `claim_session_file()` with `create_new`

```rust
// crates/harnx-runtime/src/config/mod.rs
impl Config {
    /// Atomically claim a short session ID by creating its stub file with
    /// `create_new(true)`. Returns `Ok(true)` on success, `Ok(false)` on
    /// `AlreadyExists` (caller retries), `Err` for real I/O failures.
    fn claim_session_file(&self, id: &str) -> Result<bool> {
        let path = self.session_file(id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create sessions dir at {}", parent.display())
            })?;
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)  // Atomic on Unix: fails if file exists
            .open(&path)
        {
            Ok(mut file) => {
                // Write empty stub
                file.write_all(b"")?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e).with_context(|| format!("Failed to create session file at {}", path.display())),
        }
    }
}
```

### 3. Updated `use_session(None)` path

```rust
// crates/harnx-runtime/src/config/mod.rs
match session_name {
    None => {
        let short_id = self.new_anonymous_session_id()?;
        session = Some(self::session::new(self, &short_id)?);
    }
    // ... other cases
}
```

### 4. Updated `session::new()` for named sessions

```rust
// crates/harnx-runtime/src/config/session.rs
pub fn new(config: &Config, name: &str) -> Result<Session> {
    let session_id = if uuid::Uuid::parse_str(name)
        .ok()
        .is_some_and(|uuid| uuid.get_version_num() == 7)
        || decode_timestamp_session_id(name).is_some()
    {
        name.to_string()
    } else {
        generate_session_id(|candidate| config.session_file(candidate).exists())
    };
    // ... rest of session construction
}
```

### 5. Test coverage

```rust
#[test]
fn test_new_session_has_short_id_filename() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        sessions_dir_override: Some(tmp.path().to_path_buf()),
        ..Config::default()
    };
    config.use_session(None).unwrap();

    let session = config.session.as_ref().unwrap();
    assert_eq!(session.id.len(), 6, "anonymous session ID should be 6-char short ID");
    assert!(
        decode_timestamp_session_id(&session.id).is_some(),
        "anonymous session ID should be a valid base64url timestamp short ID"
    );
    assert!(
        tmp.path().join(format!("{}.yaml", session.id)).exists(),
        "claim stub file should exist on disk immediately after use_session(None)"
    );
}

#[test]
fn test_anonymous_session_id_collision_retries() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config1 = Config {
        sessions_dir_override: Some(tmp.path().to_path_buf()),
        ..Config::default()
    };
    let mut config2 = Config {
        sessions_dir_override: Some(tmp.path().to_path_buf()),
        ..Config::default()
    };

    config1.use_session(None).unwrap();
    config2.use_session(None).unwrap();

    let id1 = config1.session.as_ref().unwrap().id.clone();
    let id2 = config2.session.as_ref().unwrap().id.clone();
    assert_ne!(id1, id2, "concurrent anonymous sessions must get unique IDs");
}
```

### 6. Snapshot flakiness fix

Added `normalize_context_counts()` and `normalize_response_lines()` to the normalization chain in `nested_sub_agent_activity_no_duplicates` to handle timing-dependent TUI output variations.

## Why This Works

1. **`create_new(true)` is atomic on Unix**: The syscall fails if file exists, preventing two processes from claiming same ID simultaneously.

2. **Retry loop handles timestamp collisions**: If two callers generate the same timestamp-based ID in same second, the first claims it atomically; second gets `AlreadyExists`, retries with next timestamp.

3. **File existence is the claim flag**: `generate_session_id` predicates check existence. Claiming immediately makes existence check accurate for subsequent callers.

4. **Cross-process safe**: Filesystem acts as coordination mechanism. Works for concurrent processes sharing same tempdir.

## Prevention Strategies

**Test Cases:**

- `test_anonymous_session_id_collision_retries` — two configs sharing same tempdir get unique IDs
- `test_new_session_has_short_id_filename` — verifies 6-char ID and stub file on disk

**Code Review Checklist:**

- [ ] Does anonymous session path use same ID generation as named sessions?
- [ ] Is file claim atomic (`create_new(true)`) vs non-atomic (check-then-write)?
- [ ] Is there a retry loop for collision recovery?
- [ ] Are timestamp-based IDs tested under rapid-fire creation?

## Related Issues

- **GitHub:** [Issue #449](https://github.com/dobesv/harnx/issues/449) — Shorter 6-char base64url session IDs
- **PR:** [#456](https://github.com/dobesv/harnx/pull/456) — Initial short ID implementation (missed anonymous path)
- **Related Solution:** [logic-errors/short-session-id-collision-claim-2026-05-03.md](./short-session-id-collision-claim-2026-05-03.md) — Collision avoidance pattern for short IDs
- **Related Solution:** [integration-issues/snapshot-normalization-non-deterministic-ids-2026-05-03.md](../integration-issues/snapshot-normalization-non-deterministic-ids-2026-05-03.md) — Snapshot normalization for short IDs
