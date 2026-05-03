---
title: "Session picker with multi-factor sorting and header-only parsing"
date: 2026-05-02
category: "integration-issues"
problem_type: integration_issue
component: "harnx-runtime/config, harnx-tui"
root_cause: "No mechanism to rank sessions by relevance or extract metadata without full file load"
resolution_type: code_fix
severity: medium
tags:
  - session-management
  - tui
  - yaml-parsing
  - uuidv7
  - terminal-fingerprinting
plan_ref: "issue-422-session-handling-revamp"
---

## Problem

TUI startup had no intelligent session selection. All sessions displayed in arbitrary order with no context awareness. Users manually found their session among dozens. Session files required full load to read metadata, making picker slow for large session directories.

## Symptoms

- Sessions listed in filesystem order (unpredictable)
- No indication which session belonged to current terminal, branch, or project
- Picker showed all sessions regardless of agent context
- Loading session metadata required parsing entire YAML file

## Investigation Steps

Issue #422 specified requirements:
1. Use UUIDv7 for session IDs (lexicographically sortable by timestamp)
2. Capture metadata: working directory, git remote, branch, terminal fingerprint
3. TUI prompts for agent/session when not specified
4. Multi-tier sort: terminal match > branch match > cwd match > remote match > recency

Implemented in phases:
1. Added `uuid` crate with `v7` feature; used `Uuid::now_v7().to_string()` for anonymous session filenames
2. Extended `SessionLogEntry::Header` with metadata fields (all optional, `#[serde(default)]` for backward compat)
3. Built `terminal_session_id()` utility checking env vars in priority order
4. Created header-only read: `read_session_header_bytes()` reads max 64KB until YAML document boundary
5. Implemented `sort_sessions_for_picker()` with tuple comparison for stable multi-tier sort
6. Added `AgentPicker` and `SessionPicker` modal states with keyboard navigation

## Root Cause

No prior mechanism existed for:
- Context-aware session ranking (terminal, git, project)
- Efficient metadata extraction (full file parse required)
- UUIDv7-based temporal ordering
- Terminal session fingerprinting across diverse terminals

## Solution

### UUIDv7 Session Filenames

Anonymous sessions use UUIDv7 as filename stem for temporal sortability:

```rust
// crates/harnx-runtime/src/config/session.rs
let session_id = Uuid::now_v7().to_string();
let filename = format!("{}.yaml", session_id);
```

Named sessions (via `-s name`) still supported; UUID stored in `session_id` header field.

### Session Metadata in Header

Extended `SessionLogEntry::Header` with optional metadata fields:

```rust
// crates/harnx-core/src/session.rs
Header {
    // ... existing fields ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_session_id: Option<String>,
}
```

All fields use `#[serde(default)]` for backward compatibility with existing session files.

### Terminal Fingerprinting

Priority-ordered env var checks with Linux fallback:

```rust
// crates/harnx-runtime/src/utils/terminal_session.rs
pub fn terminal_session_id() -> Option<String> {
    // Priority order:
    // 1. TERM_SESSION_ID → "TERM_SESSION_ID:{val}"
    // 2. WT_SESSION → "WT_SESSION:{val}"
    // 3. KITTY_WINDOW_ID → "KITTY_WINDOW_ID:{val}"
    // 4. TMUX_PANE → "tmux:{val}"
    // 5. STY → "screen:{val}"
    // Linux fallback: sid:{sid}:{tty}:{starttime}
}
```

### Header-Only Read

Parse only the first YAML document:

```rust
// crates/harnx-runtime/src/config/session_meta.rs
fn read_session_header_bytes(path: &Path) -> Option<String> {
    const MAX_HEADER_BYTES: usize = 65536;
    let mut buffer = vec![0_u8; MAX_HEADER_BYTES];
    let read_len = file.read(&mut buffer).ok()?;
    let bytes = &buffer[..read_len];

    // Find boundary: "\n---\n" (start-of-line) or "\n---\r\n"
    let boundary = content
        .windows(5)
        .position(|w| w == b"\n---\n")
        .map(|pos| pos + 1)
        .or_else(|| content.windows(6).position(|w| w == b"\n---\r\n").map(|pos| pos + 1))
        .unwrap_or(content.len());

    String::from_utf8(content[..boundary].to_vec()).ok()
}
```

**Critical insight**: Boundary must be `\n---\n` (start-of-line), not just `---\n`, to avoid false matches inside multiline YAML strings. Test `test_parse_session_meta_multiline_yaml_separator` validates this edge case.

### Multi-Tier Sorting

Sort tuple ensures stable priority ordering:

```rust
// crates/harnx-runtime/src/config/session_meta.rs
pub fn sort_sessions_for_picker(sessions: Vec<SessionMeta>, context: &PickerContext) -> Vec<SessionMeta> {
    let mut sessions = sessions;
    sessions.sort_by_key(|session| {
        (
            if session.terminal_session_id == context.current_terminal_id { 0 } else { 1 },
            if session.git_branch == context.current_branch { 0 } else { 1 },
            if session.working_dir.as_deref() == Some(context.current_dir.as_str()) { 0 } else { 1 },
            if session.git_remote == context.current_remote { 0 } else { 1 },
            session_recency_key(session),  // UUIDv7 timestamp, descending
        )
    });
    sessions
}
```

### TUI Picker Modals

`resolve_initial_modal()` fires at startup when no CLI flags set:

```rust
// crates/harnx-tui/src/lifecycle.rs
pub(crate) fn resolve_initial_modal(config: &GlobalConfig) -> Option<ModalState> {
    let agents = list_agents();
    if config.read().agent.is_none() && !agents.is_empty() {
        return Some(ModalState::AgentPicker { agents, selected: 0 });
    } else if config.read().session.is_none() {
        let sessions = config.read().list_sessions_with_meta();
        // Filter by agent, sort by context...
        return Some(ModalState::SessionPicker { sessions: sorted, selected: 0 });
    }
    None
}
```

## Why This Works

1. **UUIDv7** encodes timestamp in sort-friendly format; extracting from filename enables recency sort without file access
2. **Header-only read** limits I/O to 64KB max; YAML document boundary detection is deterministic
3. **Multi-tier sort** gives predictable ranking: sessions from current terminal/tab rank highest, then same branch, then same project
4. **Terminal fingerprinting** works across macOS Terminal, Windows Terminal, Kitty, tmux, screen, and Linux VT
5. **Backward compat** via `#[serde(default)]` on all new fields

## Prevention Strategies

**Test Cases:**
- `test_parse_session_meta_multiline_yaml_separator` — validates `\n---\n` boundary detection
- `test_sort_priority_*` — verify each tier of multi-factor sort
- `test_read_header_large_but_within_64kb` — ensure large headers parse correctly

**Key Learnings:**
- YAML document separator `---` must be matched at start-of-line (`\n---\n`) to avoid false positives in multiline strings
- `git_branch()` returns empty String (not Option) on failure — callers must convert to None explicitly
- Picker modal must be restored from `take()` if subsequent operation (`use_agent_by_name`, `use_session`) fails
- Subagents create scratch files in project root — clean before committing

**Code Review Checklist:**
- [ ] YAML boundary detection uses start-of-line pattern
- [ ] Modal restored on fallible operation failure
- [ ] Empty string failures converted to None where Option expected
- [ ] Sort tuple ordering matches priority spec

## Related Issues

- **GitHub:** [Issue #422](https://github.com/dobesv/harnx/issues/422) — Session handling revamp
- **Files Changed:**
  - `crates/harnx-core/src/session.rs` — Header metadata fields
  - `crates/harnx-runtime/src/config/session_meta.rs` — Header parsing, multi-tier sort
  - `crates/harnx-runtime/src/utils/terminal_session.rs` — Terminal fingerprinting
  - `crates/harnx-tui/src/input.rs` — Picker key handling
  - `crates/harnx-tui/src/lifecycle.rs` — Modal resolution at startup
