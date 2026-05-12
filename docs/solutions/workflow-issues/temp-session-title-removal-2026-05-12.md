---
title: "Remove defunct temp session and title generation machinery"
date: 2026-05-12
category: "workflow-issues"
problem_type: workflow_issue
component: "harnx-core, harnx-runtime"
root_cause: "Legacy temp session concept superseded by generated IDs; title generation machinery became unreachable dead code"
resolution_type: code_fix
severity: medium
tags:
  - dead-code-removal
  - session-management
  - refactoring
  - legacy-cleanup
plan_ref: "harnx-session-autoname"
---

## Problem

The session title generation feature (`SessionTitle`, formerly `AutoName`) was designed to generate human-readable titles for `"temp"` sessions. However, the concept of a `"temp"` session was legacy — superseded by modern generated session IDs. The title generation machinery had become unreachable dead code that complicated the session model.

## Symptoms

- `SessionTitle` struct and supporting methods existed but were only triggered for `"temp"` sessions
- `TEMP_SESSION_NAME` constant (`"temp"`) was a legacy sentinel never used in modern session creation
- `resolve_save_path()` contained special `_` subdirectory logic for temp sessions that was never exercised
- `GeneratingTitle` event variant defined but never emitted in practice
- `CREATE_TITLE_AGENT` re-exported from `harnx-runtime` but unused
- Session save path logic had two branches: `{sessions_dir}/{session.id}.yaml` for normal sessions vs `_/{datetime-slug}.yaml` for temp sessions — the latter never triggered

## Investigation Steps

1. Initial task was rename: `AutoName` → `SessionTitle` to improve discoverability
2. Rename completed: structs, methods, event variant (`Autonaming` → `GeneratingTitle`), user-facing messages
3. Post-rename audit revealed the feature was only partially reachable — trigger condition checked for `TEMP_SESSION_NAME` which was never set by modern session creation paths
4. Analyzed session creation flow: `harnx -a <agent> --session` bare flag generated `"temp"` sentinel session instead of real session ID
5. Recognized the entire `"temp"` session concept was legacy — modern approach generates real session IDs immediately
6. Decision: remove entire feature rather than fix trigger conditions

## Root Cause

Session model had evolved to always generate real session IDs via `new_session_id()`, but legacy `"temp"` session support remained. The title generation feature was built to provide nice names for these temp sessions, but:

1. Temp sessions were a transitional pattern now obsolete
2. Generated IDs (e.g., `abc123`) are sufficient for session identification
3. The special `_` subdirectory convention for temp sessions was never documented or consistently used
4. Title generation added complexity without current value

## Solution

Removed the entire temp session and title generation machinery:

**Removed from `harnx-core/src/session.rs`:**
```rust
// Deleted
pub const TEMP_SESSION_NAME: &str = "temp";

pub struct SessionTitle { ... }
pub session_title: Option<SessionTitle>
pub fn need_session_title(&self) -> bool
pub fn set_generating_title(&mut self, generating: bool)
pub fn chat_history_for_title_generation(&self) -> Option<String>
pub fn session_title(&self) -> Option<&str>
pub fn set_session_title(&mut self, value: &str)
```

**Removed from `harnx-core/src/event.rs`:**
```rust
// Deleted
SessionEvent::GeneratingTitle,
```

**Removed from `harnx-runtime`:**
```rust
// Deleted re-export
pub use harnx_core::session::CREATE_TITLE_AGENT;

// Removed from resolve_save_path()
// Special _/ subdirectory logic for temp sessions
// Datetime-slug naming for temp sessions
```

**Updated session creation:**
```rust
// Before: bare --session flag created "temp" sentinel
if args.session.is_some() {
    session_name = TEMP_SESSION_NAME.to_string();
}

// After: bare --session flag generates real ID immediately
if args.session.is_some() {
    session_name = new_session_id();
}
```

**Unified session save path:**
```rust
// All sessions now save as:
// {sessions_dir}/{session.id}.yaml

// No more special cases for temp sessions
// No more _/ subdirectory
// No more datetime-slug naming
```

## Why This Works

Removing dead code simplifies the codebase:

1. **One session path**: All sessions follow the same save path pattern
2. **No sentinel values**: No need to check for `"temp"` session name anywhere
3. **Real IDs immediately**: Session IDs are stable from the moment of creation
4. **Less state**: No `session_title` field, no title generation state machine
5. **Cleaner event model**: Fewer event variants to maintain

The title generation feature was well-intentioned but addressed a problem (naming temp sessions) that no longer exists. Modern session IDs serve the identification purpose adequately.

## Learnings

1. **Rename as exploration**: The initial `AutoName` → `SessionTitle` rename made the feature's scope visible enough to recognize it should be removed rather than maintained.

2. **Legacy sentinels accumulate**: Constants like `TEMP_SESSION_NAME` often indicate transitional patterns that should be removed once superseded. If modern code never sets the sentinel, consider removing the sentinel and the code that checks for it.

3. **Partially-reachable features are dead code**: If a feature only triggers under conditions that modern code paths never produce, it's effectively dead code — remove it.

4. **Simplification over feature-drift maintenance**: Attempting to "fix" the trigger condition for title generation would have added complexity to a legacy pattern. Removing the feature simplified the system.

## Prevention Strategies

**During Model Refactors:**
- Audit for sentinel values and transitional patterns in the old model
- Check if code checking for those sentinels is still reachable
- Remove or update unreachable branches rather than leaving them

**Code Review Checklist:**
- [ ] Does this constant represent a transitional/legacy pattern?
- [ ] Is code that checks for legacy patterns still reachable from modern paths?
- [ ] Would removing the legacy pattern simplify the model?

**Health Checks:**
- Grep for sentinel values like `"temp"`, `DEFAULT_*`, `LEGACY_*` — verify they're still set somewhere
- Check conditionals for feature trigger conditions — are they reachable from all entry points?

## Related Issues

- **GitHub:** [Issue #502](https://github.com/dobesv/harnx/issues/502) — Rename confusing "autoname" session feature
- **Related Solution:** [removing-dead-config-fields-rust-2026-05-05.md](removing-dead-config-fields-rust-2026-05-05.md) — Dead field removal checklist
