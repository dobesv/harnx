---
title: "TUI transcript blanked out by compaction summary collapse"
date: 2026-07-04
category: logic-errors
problem_type: logic_error
component: "harnx-tui"
root_cause: "compaction rebuilt transcript with compressed history hidden inside CompactionMarker.detail_text and System summary silently dropped by messages_to_transcript_items filter"
resolution_type: code_fix
severity: high
tags:
  - tui
  - ratatui
  - transcript
  - compaction
  - visibility
  - event-boundary
plan_ref: issue-904-compaction-transcript
---

## Problem

After session compaction completed, the TUI transcript appeared completely empty even though the session history was intact. Prior conversation vanished into a single collapsed line.

## Symptoms

```
- User runs `.compact session` (or auto-compaction triggers)
- Compaction completes successfully
- Transcript shows only: `─── session compacted ───`
- All prior user/assistant messages disappear
- Opening the compaction marker detail view showed archived messages, but inline transcript was blank
- Compaction summary text (leading System message) was not visible anywhere
```

## Investigation Steps

1. Traced `SessionEvent::CompactingCompleted` handler in `lifecycle.rs` — it calls `session_history_transcript_items()` to rebuild transcript.

2. Found `session_history_transcript_items()` built transcript as:
   - `[CompactionMarker{ detail_text = <all compressed messages collapsed> }]` + preserved suffix
   - Inline transcript showed only the marker text, not the archived messages

3. Discovered `messages_to_transcript_items()` ignores `MessageRole::System` messages — the compaction summary (prepended by `compress_keeping_recent` as `active_messages[0]`) was silently dropped.

4. Identified two independent bugs:
   - Compressed history was hidden, not rendered inline
   - System summary was swallowed by filter

5. Review flagged coverage gaps: non-empty summary path and remote path untested.

## Root Cause

The compaction rebuild treated archived messages as hidden detail content rather than visible transcript history. The `CompactionMarker.detail_text` field aggregated all compressed messages into invisible storage. Additionally, the generic `messages_to_transcript_items()` filter that drops System messages swallowed the compaction summary, which by invariant sits at `active_messages[0]` after `compress_keeping_recent`.

**Key invariant violated:** Compaction is an event boundary, not a replacement. Prior history should remain visible with a marker separating archived from active.

## Solution

### 1. Model compaction as inline event boundary

Rebuild transcript structure: `[...compressed_messages as visible items, CompactionMarker, ...preserved suffix]`

```rust
pub(crate) fn build_transcript_with_compaction(
    compressed_messages: &[Message],
    active_messages: &[Message],
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    if !compressed_messages.is_empty() {
        // Render archived history inline, not hidden in detail_text
        items.extend(messages_to_transcript_items(compressed_messages, decl_map));
        
        // Extract summary from leading System message (invariant: compress_keeping_recent prepends it)
        let summary_text = active_messages
            .first()
            .filter(|msg| msg.role == MessageRole::System)
            .map(|msg| msg.content.to_text())
            .unwrap_or_default();
        
        items.push(build_compaction_marker(compressed_messages, summary_text, decl_map));
    }
    items.extend(messages_to_transcript_items(active_messages, decl_map));
    items
}
```

### 2. Surface System summary on the marker

Add `summary_text` field to `CompactionMarker`:

```rust
// types.rs
CompactionMarker {
    text: String,           // "─── session compacted ───"
    summary_text: String,    // Compaction summary (extracted from System message)
    from_seq: Option<usize>,
    to_seq: Option<usize>,
    detail_text: String,     // Full archived content for detail view
}
```

Render inline with summary:

```rust
// render.rs
TranscriptItem::CompactionMarker { text, summary_text, .. } => {
    let mut lines = Self::render_text_entry("", text, dim_style, false);
    if !summary_text.is_empty() {
        lines.extend(Self::render_text_entry("", summary_text, dim_style, false));
    }
    RenderedEntry::from_lines(lines, width)
}
```

### 3. Shared helper for local/remote parity

Extract `build_transcript_with_compaction()` as `pub(crate)` helper called by BOTH:
- Local path: `session.compressed_messages`, `session.messages`
- Remote path: `load_remote_transcript_for_render()` result

This prevents layout drift and makes remote logic unit-testable directly.

```rust
// Both paths now identical:
build_transcript_with_compaction(&compressed, &active, &decl_map)
```

### 4. Update footprint accounting

Include `summary_text.len()` in size tracking:

```rust
// lifecycle.rs transcript_footprint
TranscriptItem::CompactionMarker { text, summary_text, detail_text, .. } => {
    text.len() + summary_text.len() + detail_text.len()
}
```

## Why This Works

1. **Event boundary model**: Archival is transparent — prior history stays visible with marker as separator
2. **Summary surfacing**: System message is extracted and rendered on marker, not swallowed by generic filter
3. **Shared helper**: Local and remote paths converge — no code duplication, testable directly
4. **Invariant documented**: Code comment explains that `active_messages[0]` being System is contract from `compress_keeping_recent`

## Prevention Strategies

**Test Cases:**
- `session_history_compaction_marker_carries_summary_text`: Seed session with System summary at active[0]; assert marker carries it and no duplicate System row appears
- `compaction_marker_summary_rendered_inline`: Screen render assertion shows summary text below marker line
- `build_transcript_with_compaction_orders_history_marker_and_suffix`: Direct unit test of shared helper — ordering, single marker, summary + from/to_seq populated, empty-compressed => no marker

**Seed helper pattern:**
```rust
/// Active window begins with System summary (as produced by compress_keeping_recent)
fn seed_compressed_session_with_summary(config: &GlobalConfig) {
    session.compressed_messages = vec![/* archived turns */];
    session.messages = vec![
        make_compacted_message(MessageRole::System, "Summary...", 2),
        make_compacted_message(MessageRole::User, "Fresh question", 3),
    ];
}
```

**Code Review Checklist:**
- [ ] Does `messages_to_transcript_items()` silently drop messages? Document which roles are filtered and why
- [ ] When adding TranscriptItem variants with summary/boundary semantics, ensure they render inline
- [ ] Do local and remote transcript paths call the same builder? Extract shared helper if duplicated
- [ ] Is the `active_messages[0] is System` invariant documented where it's consumed?

## Related Issues

- **Issue:** [#904](https://github.com/dobesv/harnx/issues/904) — TUI transcript blanked out by compaction
- **Prior Solution:** [tui-compaction-marker-detail-view-2026-06-12.md](../integration-issues/tui-compaction-marker-detail-view-2026-06-12.md) — Marker as navigable event with detail view
- **Prior Solution:** [tui-compaction-spinner-corruption-2026-06-08.md](../integration-issues/tui-compaction-spinner-corruption-2026-06-08.md) — AgentEvent-based compaction progress
