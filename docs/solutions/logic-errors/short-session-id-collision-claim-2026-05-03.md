---
title: "Short session ID collision prevention via immediate file claim"
date: 2026-05-03
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime/config, harnx-acp-server"
root_cause: "Collision check uses file existence; rapid session creation within same second produces duplicate IDs before file is written"
resolution_type: code_fix
severity: medium
tags:
  - session-management
  - concurrency
  - timestamp-based-ids
  - file-locking
plan_ref: "session-management-451-450-448-449"
---

## Problem

Replacing UUID-based session IDs with shorter 6-character base64url timestamps caused duplicate session IDs when creating sessions rapidly back-to-back within the same second. The collision check uses file existence, but the session file is not written until after ID generation completes.

## Symptoms

- Duplicate session IDs created when `new_session` called in quick succession (e.g., automated tests, ACP server batch operations)
- Session files overwritten or lost
- Race condition: both sessions write to the same file path

## Root Cause

The `generate_session_id` function checks `exists(candidate)` then returns the candidate if it doesn't exist. But the caller (`Session::new`) only writes the session file later. Between ID generation and file write, another caller can generate the same ID:

```
Thread A: generate_session_id() -> "afgkKA" (doesn't exist)
Thread B: generate_session_id() -> "afgkKA" (doesn't exist yet!)
Thread A: write session file "afgkKA.yaml"
Thread B: write session file "afgkKA.yaml" <- OVERWRITE
```

The 6-character ID encodes Unix seconds. Multiple calls within the same second produce the same candidate string.

## Solution

The ID generation happens in `session_name.rs`, but the file claim must happen immediately after. The fix is in the caller: write the session file immediately after generating the ID, before any other work.

**Implementation pattern** (in `use_session`):

```rust
// crates/harnx-runtime/src/config/mod.rs
pub fn use_session(&mut self, name: Option<&str>) -> Result<()> {
    let session = if let Some(name) = name {
        // Load existing session
        session::load(self, name, &path)?
    } else {
        // Create new session - this generates ID and writes file
        session::new(self, "")?
    };
    self.session = Some(session);
    // ... rest of setup
}
```

The key is that `session::new()` must persist the session file before returning:

```rust
// crates/harnx-runtime/src/config/session.rs
pub fn new(config: &Config, name: &str) -> Result<Session> {
    let session_id = generate_session_id(|candidate| config.session_file(candidate).exists());

    let mut session = Session {
        id: session_id.clone(),
        session_id: Some(session_id),
        // ... fields ...
    };
    session.set_agent(&agent)?;

    // CLAIM THE ID: write file immediately
    let path = config.session_file(&session.id);
    std::fs::write(&path, "")?;  // Empty stub; will be rewritten with content

    Ok(session)
}
```

For ACP server's `new_session`, the pattern is:

```rust
// crates/harnx-acp-server/src/lib.rs
async fn new_session(&self, _args: acp::NewSessionRequest) -> acp::Result<acp::NewSessionResponse> {
    let mut config = self.config.write();
    config.use_agent_by_name(&self.agent_name)?;
    config.use_session(None)?;  // This calls session::new() which claims ID
    let session_id = config.session.as_ref().unwrap().id.clone();
    // ... register session
}
```

## Why This Works

1. **File existence as claimed flag**: The collision check in `generate_session_id` uses `exists(candidate)`. Writing the file immediately after ID generation "claims" the ID atomically on Unix filesystems (O_CREAT is atomic).

2. **Retry loop handles collisions**: If another process claimed the ID, `generate_session_id` increments the timestamp by 1 second and tries again.

3. **No external locking needed**: Filesystem acts as the coordination mechanism. Works across processes, not just threads.

## Prevention Strategies

**Test Cases:**

```rust
#[test]
fn test_new_session_returns_unique_ids() {
    // Create sessions in rapid succession within same second
    let resp1 = agent.new_session(req).await?;
    let resp2 = agent.new_session(req).await?;
    assert_ne!(resp1.session_id, resp2.session_id);
}
```

**Code Review Checklist:**

- [ ] Does ID generation happen in the same critical section as file creation?
- [ ] Is there a window between ID generation and file write where another caller could produce the same ID?
- [ ] Are timestamp-based IDs tested under rapid-fire creation?

## Related Issues

- **GitHub:** [Issue #449](https://github.com/dobesv/harnx/issues/449) — Shorter 6-char base64url session IDs
- **Related Solution:** [integration-issues/session-picker-multi-factor-sorting-2026-05-02.md](../integration-issues/session-picker-multi-factor-sorting-2026-05-02.md) — Session ID generation and picker sorting
