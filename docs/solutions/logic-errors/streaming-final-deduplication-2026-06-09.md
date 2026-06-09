---
title: "Fix duplicated assistant text from streaming + final message"
date: 2026-06-09
category: "logic-errors"
problem_type: logic_error
component: "streaming-event-handlers"
root_cause: "dual-finalization in streaming mode"
resolution_type: code_fix
severity: medium
tags:
  - streaming
  - duplicate-text
  - event-handling
  - tui
  - acp-server
plan_ref: "issue-671-dup-text"
---

## Problem

Streaming models emit incremental `ModelEvent::MessageChunk` events, then a `ModelEvent::Final`. Consumers that also render `Final.output` duplicate the already-streamed text. Reported in harnx TUI and kagent (ACP client).

## Symptoms

```
- Behavior: Assistant text appears twice in TUI for multiline streaming responses
- Trigger: Claude and other streaming models; multiline responses always duplicated
- ACP clients: kagent reported duplicate text via ACP protocol
```

## Investigation Steps

Traced event flow through TUI and ACP server:

1. Engine streaming path (`crates/harnx-engine/src/chat_completions.rs:run_chat_completion_streaming`) correctly emits `Final { output: "" }` (empty).

2. TUI's `on_text_response` callback (`crates/harnx-tui/src/prompt.rs:124-135`) re-emits `Final { output: <full text> }` after streaming chunks already populated transcript.

3. TUI Final handler (`crates/harnx-tui/src/input.rs:1014-1067`) appends `output` as NEW `AssistantText` when `streaming_assistant_idx == None`.

4. `append_streaming_assistant_chunk` (`crates/harnx-tui/src/render.rs:615-665`) resets `streaming_assistant_idx = None` at every newline boundary. Multiline responses always trigger this, causing duplication.

5. ACP server sink (`crates/harnx-acp-server/src/lib.rs:78-93`) forwards non-empty `Final` even after chunks were forwarded — latent hazard.

## Root Cause

Two finalization systems active in streaming mode:

- **Engine**: emits empty `Final` on streaming path (correct).
- **TUI callback**: manually emits non-empty `Final` via `on_text_response` after streaming complete.
- **TUI Final handler**: appends full text as new `AssistantText` when `streaming_assistant_idx` was reset to `None`.
- **render.rs**: resets `streaming_assistant_idx = None` at newline boundaries — multiline responses always duplicate.

The `streaming_assistant_idx` tracks which transcript entry receives chunks, but resets mid-stream on newlines. Cannot be used for dedup.

## Solution

Track per-turn boolean flag `streamed_text_this_turn`. Set `true` when any non-empty `MessageChunk` rendered/forwarded. Suppress rendering/forwarding `Final.output` when flag set (chunks already cover text). Keep usage/cleanup logic unconditional.

**TUI** (`crates/harnx-tui/src/types.rs`):
```rust
pub struct App {
    // ...
    pub streamed_text_this_turn: bool,
}
```

**TUI chunk handler** (`crates/harnx-tui/src/input.rs:1004-1013`):
```rust
if !text.is_empty() {
    self.app.streamed_text_this_turn = true;
    self.append_streaming_assistant_chunk(&text);
}
```

**TUI Final handler** (`crates/harnx-tui/src/input.rs:1033-1070`):
```rust
if !output.is_empty() && !self.app.streamed_text_this_turn {
    // Only render Final.output if no chunks were streamed
    self.app.transcript.push(TranscriptItem::AssistantText { ... });
}
self.app.streamed_text_this_turn = false; // reset for next turn
```

**ACP sink** (`crates/harnx-acp-server/src/lib.rs:71-103`):
```rust
struct AcpChunkSink {
    tx: UnboundedSender<AcpForward>,
    streamed_text_this_turn: AtomicBool, // fresh per prompt
}

// On MessageChunk:
if !text.is_empty() {
    self.streamed_text_this_turn.store(true, Ordering::Relaxed);
    self.tx.send(AcpForward::Text(text, source));
}

// On Final:
if !output.is_empty() && !self.streamed_text_this_turn.load(Ordering::Relaxed) {
    self.tx.send(AcpForward::Text(output, source));
}
```

**Reset points:**
- TUI: `start_prompt` (line 1307), `Final` handler (line 1070), `clear_transcript` (lifecycle.rs:99)
- ACP: fresh `AtomicBool::new(false)` per `prompt()` call — sink created fresh each request

## Why This Works

Flag survives newline-boundary resets of `streaming_assistant_idx`. Once any chunk text rendered, Final's text suppressed for remainder of turn. Non-streaming path never sets flag, so `Final.output` still renders — only text source there.

Key insight: streaming configuration is per-prompt (`config.stream`). Every tool round in a single prompt uses same stream setting — streaming round never followed by non-streaming round within one prompt. Per-prompt sticky flag is safe.

## Prevention Strategies

**Test cases:**
- `final_does_not_duplicate_streamed_multiline_assistant_text` — TUI multiline streaming no-dup
- `final_without_chunks_renders_assistant_text_once` — TUI non-streaming still renders
- `acp_chunk_sink_skips_final_after_streamed_text` — ACP suppression

**Pattern to follow:**
- Use turn-scoped flag for dedup, not chunk-scoped index
- Reset flag at prompt/turn boundaries, not mid-stream
- ACP sink lifetime: fresh per `prompt()` call

**Code review checklist:**
- [ ] Does Final handler check streamed-text flag before rendering?
- [ ] Is flag reset at all turn boundaries?
- [ ] ACP sink: Is `AtomicBool` created fresh per request?

## Related Issues

- GitHub Issue: #671
- Follow-up: CLI sink (`crates/harnx/src/cli_event_sink.rs`) lacks same guard but currently safe because engine emits empty Final on streaming path. Revisit if provider emits non-empty Final alongside chunks.
