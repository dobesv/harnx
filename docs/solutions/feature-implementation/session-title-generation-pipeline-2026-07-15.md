---
title: "Session title generation pipeline with LLM-driven auto-titling"
date: 2026-07-15
category: "feature-implementation"
problem_type: logic_error
component: "harnx-core, harnx-runtime"
root_cause: "Non-obvious edge cases in first-title trigger, replay token derivation, and title extraction for large logs"
resolution_type: code_fix
severity: medium
tags:
  - session-management
  - title-generation
  - replay-state
  - async-pipeline
  - nats-index
  - backward-compatibility
plan_ref: "session-title-generation"
---

## Problem

Implementing automatic LLM-driven session title generation required mirroring the compaction pipeline's async pattern while handling non-obvious edge cases: first-title triggering, replay token state derivation, title extraction from large logs, manual title freeze, and NATS index propagation.

## Symptoms

Several design bugs emerged during review:

- First title never generated — pure token-delta check (`tokens - last_updated >= 50000`) stays false for normal sessions
- Large auto-titled sessions re-titled on every reload — `session.tokens` is 0 mid-replay, so deriving `title_last_updated_tokens` from it produces 0 baseline
- Manual `.set title` not reflected in local listing after reload
- Titles in middle of >128KB logs missed by bounded prefix scan
- Remote listing title lags behind local

## Investigation Steps

### 1. First-Title Trigger

`need_generate_title` checked only `tokens.saturating_sub(title_last_updated_tokens) >= threshold`. For new sessions (`tokens=1000`, `last=0`), `1000 >= 50000` is false. Fix: add `title.is_none() && tokens > 0` check first.

### 2. Mid-Replay Token State

Session log replay runs BEFORE `update_tokens()` computes the cumulative count. Any `session.tokens` read during replay is stale (0 at start, partial during iteration). Deriving `title_last_updated_tokens = session.tokens` at replay time sets it to 0 for large auto-titled sessions, causing re-generation on every reload.

Fix: persist the token count directly in the `Title` log entry:
```rust
#[serde(rename = "title")]
Title {
    title: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    tokens: usize,
    #[serde(default, skip_serializing_if = "Not::not")]
    manual: bool,
},
```

Replay restores `title_last_updated_tokens = entry.tokens` (or `usize::MAX` for manual titles).

### 3. Manual Title Freeze Across Reload

`.set title` sets `manual: true` and `tokens: usize::MAX`. Replay recognizes manual titles and sets `title_last_updated_tokens = usize::MAX` — a sentinel that never crosses the regeneration threshold.

### 4. Latest Title Extraction

Initially `scan_first_title` returned the first `Title` in 64KB prefix. This hid regeneration events and manual overrides appending later.

Fix: `scan_latest_title` reads the whole file (lossy utf8), string-splits on `---` doc boundaries, parses each chunk, returns the LAST `Title` found. A bounded window is insufficient — titles can land in the middle of large logs.

### 5. NATS Index Propagation (Option A)

`generate_title` is NATS-free — no kv::Store plumbing. The worker's existing per-activation `upsert_session_index_record` carries the latest `Title` from `effective_entries`. Tradeoff: remote listing title lags by one activation. The `update_session_title` CAS helper exists but has no caller — kept as tested public API for future use.

## Solution

### Async Title Pipeline (mirrors compaction)

```rust
// session_ops_title.rs — fire-and-forget spawn pattern
pub fn maybe_generate_title(config: GlobalConfig) {
    let threshold = config.read().data.title_update_threshold;
    let need = config.read().session.as_ref()
        .map(|s| s.need_generate_title(threshold))
        .unwrap_or(false);
    if !need { return; }
    
    config.write().session.as_mut().map(|s| s.set_titling(true));
    let session_id = config.read().session.as_ref().map(|s| s.id.clone());
    
    tokio::spawn(async move {
        let result = generate_title(&config).await;
        // Session-swap guard before mutation
        if config.read().session.as_ref().map(|s| &s.id) != session_id.as_ref() {
            return; // session swapped, abort
        }
        config.write().session.as_mut().map(|s| s.set_titling(false));
        if let Some(title) = result.ok().flatten() {
            emit_agent_event(AgentEvent::Session(SessionEvent::TitleUpdated(title)));
        }
    });
}
```

### First-Title Trigger Logic

```rust
pub fn need_generate_title(&self, threshold: usize) -> bool {
    if self.titling { return false; }
    if threshold == 0 { return false; }
    // First title: no title yet but tokens exist
    if self.title.is_none() && self.tokens > 0 { return true; }
    // Regeneration: token delta exceeds threshold
    self.tokens.saturating_sub(self.title_last_updated_tokens) >= threshold
}
```

### Title Log Entry with Token Persistence

```rust
// Generation writes tokens = session.tokens, manual = false
SessionLogEntry::Title {
    title: cleaned_title,
    tokens: session.tokens(),
    manual: false,
}

// Manual set writes tokens = session.tokens, manual = true
SessionLogEntry::Title {
    title: user_title,
    tokens: session.tokens(),
    manual: true,
}

// Replay restores state from entry, NOT from session.tokens
SessionLogEntry::Title { title, tokens, manual } => {
    session.title = Some(title);
    session.title_last_updated_tokens = if manual { usize::MAX } else { tokens };
}
```

### Latest Title Scanning

```rust
pub fn scan_latest_title(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    last_title_in_buffer(&contents)
}

fn last_title_in_buffer(contents: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for doc in contents.split("\n---").flat_map(|d| d.split("\n---\r\n")) {
        if let Ok(SessionLogEntry::Title { title, .. }) = serde_yaml::from_str(doc) {
            last = Some(title);
        }
    }
    last
}
```

### Backward Compatibility

`SessionLogEntry::Title` placed before `#[serde(other)] Unknown`. New fields use `#[serde(default)]`. Old entries without `tokens`/`manual` deserialize with defaults (0/false). `SessionIndexRecord.title` uses `#[serde(default, skip_serializing_if)]`.

## Why This Works

1. **First-title check**: Explicit `title.is_none()` triggers generation after first exchange regardless of threshold
2. **Token persistence**: `Title.tokens` captures the actual count at generation time, replay restores it directly without relying on `session.tokens` (which is stale during replay)
3. **Manual sentinel**: `usize::MAX` makes regeneration mathematically impossible — `tokens.saturating_sub(usize::MAX) >= threshold` is always false
4. **Whole-file scan**: Titles can appear mid-log; bounded windows miss them. Reading whole file is correct; perf cost is acceptable for sessions (files are session logs, not arbitrary large data)
5. **Option A propagation**: Simple — no NATS plumbing on generation path, worker handles propagation on next activation. Log is source of truth.

## Prevention Strategies

### Replay State Rules

- Never derive replay-able state from `session.tokens`, `session.modified`, or any field computed post-replay
- Persist required values directly in log entries with `#[serde(default)]` for backward compat
- Test: reload a session with many tokens, verify `title_last_updated_tokens` matches generation time not 0

### Async Pipeline Patterns

- Mirror compaction: `titling` flag, `tokio::spawn`, session-id guard against swap, flag clear in spawn
- Re-entrancy guard (`titling`) checked BEFORE spawn
- Session-swap check AFTER spawn, BEFORE mutation

### Enumeration Extraction

- Append-only events require LATEST semantics, not FIRST
- Bounded prefix scans are insufficient for events that can appear mid-file
- Test with >128KB fixture, event placed past prefix boundary

### Concurrency Safety (WORKFLOW)

- Multiple editing agents on shared working tree caused mass file deletion via git checkout/restore
- **Edit strictly serially** — one agent at a time on the same checkout
- Recover with `git reset --hard HEAD` after verified commit

## Related Issues

- **GitHub:** [Issue #103](https://github.com/dobesv/harnx/issues/103)
- **Prior removal:** [temp-session-title-removal-2026-05-12.md](../workflow-issues/temp-session-title-removal-2026-05-12.md) — explains why the earlier title system was removed
- **NATS index pattern:** [nats-kv-session-index-enumeration-2026-06-27.md](../integration-issues/nats-kv-session-index-enumeration-2026-06-27.md) — index read/write pattern
- **Async pattern:** [session-actor-concurrency-invariants-2026-07-04.md](../async-patterns/session-actor-concurrency-invariants-2026-07-04.md) — spawn/flag/guard patterns
