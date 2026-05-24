---
title: "TUI Session Breakdown on Exit — Pure Function Extraction Pattern"
date: 2026-05-18
category: logic-errors
problem_type: logic_error
component: "harnx-tui / harnx main.rs"
root_cause: "need for testable selection logic in I/O-bound function"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - transcript
  - testing-pattern
  - pure-function
  - accessor-design
plan_ref: "harnx-573-tui-exit-session-breakdown"
last_updated: 2026-05-24
---

## Problem

Issue #573 required printing a session summary (first user message, last user message if different, final LLM response) to stderr when exiting the TUI. The initial implementation placed all selection logic inside `print_session_breakdown()`, which directly printed to stderr via `MarkdownRender`. This made the selection logic untestable without capturing output.

## Symptoms

- New transcript filtering logic in `main.rs` had no unit tests
- Blocker finding during code review: "Missing automated tests for new transcript breakdown logic"
- `print_session_breakdown()` combined I/O (stderr printing) with data transformation, violating separation of concerns

## Investigation Steps

1. Code review identified the blocker: pure data transformation logic embedded in I/O-bound function
2. Examined the selection logic: finding first/last user messages, collecting trailing assistant/thought text
3. Confirmed logic was testable if extracted: no dependencies on external state
4. Refactored by splitting into:
   - `select_breakdown_sections(transcript) -> Option<BreakdownSections>` — pure function
   - `print_session_breakdown(transcript, source)` — I/O function calling selector
5. Added unit tests for the extracted selector

## Root Cause

The need to perform user-visible I/O on TUI exit tempted initial implementation to inline all logic in the print function. Testability required extracting the data transformation into a pure function that:
- Takes immutable transcript slice
- Returns a struct of selected text sections
- Has no side effects

## Solution

**Pattern: Extract pure selection logic from I/O-bound functions**

Created `BreakdownSections` struct and `select_breakdown_sections()` function:

```rust
struct BreakdownSections<'a> {
    first_user: &'a str,
    last_user: Option<&'a str>,
    final_response: Vec<&'a str>,
}

fn select_breakdown_sections(transcript: &[TranscriptItem]) -> Option<BreakdownSections<'_>> {
    let first_user_idx = transcript
        .iter()
        .position(|item| matches!(item, TranscriptItem::UserText { .. }))?;
    let last_user_idx = transcript
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::UserText { .. }))?;

    let first_user = transcript_item_text(&transcript[first_user_idx])?;
    let last_user = (last_user_idx != first_user_idx)
        .then(|| transcript_item_text(&transcript[last_user_idx]))
        .flatten();
    let final_response = transcript
        .iter()
        .skip(last_user_idx + 1)
        .filter_map(|item| match item {
            TranscriptItem::AssistantText { text, .. } | TranscriptItem::ThoughtText(text) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();

    Some(BreakdownSections { first_user, last_user, final_response })
}
```

**Accessor pattern: Read-only transcript exposure**

Added `transcript()` method to `Tui` returning immutable slice:

```rust
impl Tui {
    pub fn transcript(&self) -> &[TranscriptItem] {
        &self.app.transcript
    }
}
```

This enables external consumers (CLI) to read transcript data without mutation capability.

**Test coverage added:**

- Empty transcript returns `None`
- Single user message: no last_user, no final_response
- Multiple users: correctly identifies first and last
- Trailing assistant/thought collected in order
- Non-response items (SystemText, ToolCall) excluded from final_response
- Immediate exit (no response yet): empty final_response

## Why This Works

1. **Pure function extraction** separates data transformation from I/O, enabling direct unit tests
2. **Struct return type** captures all results in a single value, easy to assert against
3. **Iterator methods** (`position`, `rposition`, `skip`, `filter_map`) provide clean, idiomatic filtering
4. **Read-only accessor** on `Tui` exposes internal state safely without creating mutation paths
5. **Public re-export of `TranscriptItem`** allows external crate to match on variants needed for filtering

## Known Technical Debt

### `source_heading` Duplication (4 locations)

The `source_heading(AgentSource) -> String` function is duplicated in:
- `crates/harnx/src/main.rs:356`
- `crates/harnx/src/cli_event_sink.rs:40`
- `crates/harnx-tui/src/render_helpers.rs:41` (`pub(crate)`)
- `crates/harnx-acp/src/client.rs:1068`

All implementations are identical. Future refactor should move to shared location (e.g., method on `AgentSource` in `harnx-core`).

### Minor Issues (Non-blocking)

- ~~`RenderOptions::default()` used instead of config-derived options~~ — **Fixed 2026-05-24** (see Follow-up Fixes below)
- Single `MarkdownRender` instance reused across items (state carry risk for malformed markdown)
- Double `config.read()` in `start_interactive()` when building `AgentSource`

## Follow-up Fixes (2026-05-24)

Two bugs reported after PR #595 merge, addressed in plan `harnx-573-tui-exit-session-breakdown-fixes`:

### Bug 1: Markdown Not Rendered on TUI Exit

**Problem**: `print_session_breakdown` initialized `MarkdownRender` with `RenderOptions::default()` (theme: None). Without a theme, `MarkdownRender::highlight_line` returns lines unchanged — raw markdown displayed to user.

**Fix**: Pass `&GlobalConfig` to `print_session_breakdown` and use `config.read().render_options().unwrap_or_default()`. Matches pattern used elsewhere (`main.rs:228`, `cli_event_sink.rs`).

**Pattern**: `MarkdownRender` with `RenderOptions::default()` silently produces unformatted output. Always use `config.read().render_options().unwrap_or_default()` for user-facing rendering.

### Bug 2: ThoughtText Dumped in Final Response

**Problem**: `select_breakdown_sections` collected both `TranscriptItem::AssistantText` and `TranscriptItem::ThoughtText` into `final_response`. Owner reported too much output.

**Fix**: Filter to `AssistantText` only. Unit tests in `main.rs` directly test transcript item filtering — straightforward to extend.

**Key insight**: `ThoughtText` contains internal reasoning, not user-facing responses. Include only `AssistantText` in session breakdown output.

## Prevention Strategies

**For similar features requiring session data extraction:**

1. Design pure selector function first, then wire into I/O path
2. Use `&[TranscriptItem]` as input to enable both TUI and test contexts
3. Return `Option<Struct>` for clean early-exit handling
4. Write tests for edge cases before implementing print logic

**Test cases to include:**
- Empty input
- Single item
- Multiple items with marker variants
- Items that should be excluded from selection
- Order preservation in collected results

**Code review checklist:**
- [ ] Is selection logic extracted and testable?
- [ ] Does accessor return read-only reference for external consumers?
- [ ] Are iterator methods (`position`/`rposition`) used over manual indexing?
- [ ] Is there a test asserting empty collection handling?
- [ ] Does `MarkdownRender` use config-derived `RenderOptions` (not `default()`)?
- [ ] Are only user-facing transcript items included in output (exclude `ThoughtText`)?

## Related Issues

- **Issue:** [#573](https://github.com/dobesv/harnx/issues/573) — On exit print some excerpts from session
- **PR:** [#595](https://github.com/dobesv/harnx/pull/595) — Print session breakdown on TUI exit (initial implementation)
- **Plan:** `harnx-573-tui-exit-session-breakdown-fixes` — Follow-up bug fixes (2026-05-24)
- **Related:** `logic-errors/session-resume-hint-on-exit-2026-05-05.md` — Similar stderr output pattern on TUI exit
