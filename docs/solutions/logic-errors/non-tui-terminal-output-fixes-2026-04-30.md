---
title: "Non-TUI terminal output fixes: heading repetition and raw-mode corruption"
date: 2026-04-30
category: logic-errors
problem_type: logic_error
component: cli-event-sink
root_cause: "missing source tracking and unnecessary raw-mode in non-TUI path"
resolution_type: code_fix
severity: high
tags:
  - terminal
  - streaming
  - raw-mode
  - state-tracking
  - line-buffering
plan_ref: harnx-410-414-terminal-output-fixes
---

## Problem

Two independent bugs in `crates/harnx/src/cli_event_sink.rs` corrupted non-TUI CLI output:

1. **Issue #410:** Source heading printed before every streaming chunk, breaking word-level streaming into separate lines each with its own heading prefix.
2. **Issue #414:** Raw mode enabled during markdown rendering path; crashes left terminal stuck in raw mode.

## Symptoms

**Issue #410:**
- Streaming output like "APPROVE" appeared as separate lines: "AP", "P", "R", "OVE", each prefixed with the agent/session heading
- User saw: `[my-agent] AP\n[my-agent] P\n[my-agent] R\n[my-agent] OVE` instead of `[my-agent] APPROVE`

**Issue #414:**
- Terminal left in raw mode after crash or early exit
- User had to run `reset` or `stty cooked` to restore normal terminal behavior
- Ctrl-C delivered as raw key event rather than SIGINT

## Investigation Steps

Traced both issues to `cli_event_sink.rs`:

1. **For #410:** Found `emit()` called `cleanup()` and printed source heading on every `MessageChunk`/`ThoughtChunk` event. The `show_heading` logic matched ALL model output events, not just the first per source.

2. **For #414:** Found `handle_markdown_chunk` enabled raw mode for cursor-position-based live re-rendering, mirroring TUI behavior. Non-TUI path doesn't need cursor manipulation — `print!`/`println!` work fine in cooked mode.

## Root Cause

**Issue #410:** No tracking of "which source last produced output". Each `emit()` call independently decided to show heading based on event type alone, without considering whether heading was already printed for this source.

**Issue #414:** Raw mode unnecessary for non-TUI output. Cooked mode handles line-buffered `print!`/`println!` correctly, and Ctrl-C delivers SIGINT normally without a key watcher.

## Solution

### Fix for #410: Source Tracking

Added `last_ui_output_source: Option<AgentSource>` to `CliSinkState`. Heading + cleanup only run when `source != last_ui_output_source`:

```rust
// cli_event_sink.rs
struct CliSinkState {
    // ... other fields ...
    last_ui_output_source: Option<AgentSource>,
}

impl AgentEventSink for CliAgentEventSink {
    fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
        // ...
        if is_model_output {
            let next_source = source.as_ref();
            if next_source != state.last_ui_output_source.as_ref() {
                state.cleanup()?;
                if let Some(source) = next_source {
                    println!("{}", source_heading(source));
                }
                state.last_ui_output_source = next_source.cloned();
            }
        }
        // ...
    }
}
```

`cleanup()` resets `last_ui_output_source` to `None` so next turn shows its heading.

### Fix for #414: Line-Buffered Cooked Mode

Replaced raw-mode cursor-aware rendering with line-buffered approach:

```rust
// cli_event_sink.rs
fn handle_markdown_chunk(&mut self, text: &str) -> anyhow::Result<()> {
    // Initialize renderer lazily
    if self.render.is_none() {
        self.render = Some(MarkdownRender::init(self.render_options.clone())?);
    }

    let text = text.replace('\t', "    ");
    self.buffer.push_str(&text);

    if self.buffer.contains('\n') {
        let buffer = std::mem::take(&mut self.buffer);
        let (head, tail) = split_line_tail_local(&buffer);
        let render = self.render.as_mut().expect("initialized above");
        let output = render.render(head);
        // render() joins lines with '\n' but adds NO trailing newline.
        // println! adds the separator between rendered head and next chunk.
        println!("{output}");
        self.buffer = tail.to_string();
    }

    stdout().flush()?;
    Ok(())
}

fn cleanup(&mut self) -> anyhow::Result<()> {
    // Flush partial line still in buffer
    if !self.buffer.is_empty() {
        let tail = std::mem::take(&mut self.buffer);
        let rendered = self.render.as_mut().map(|r| r.render(&tail)).unwrap_or(tail);
        println!("{rendered}");
    }
    self.buffer.clear();
    self.render = None;
    self.last_ui_output_source = None;
    Ok(())
}
```

Helper function:

```rust
fn split_line_tail_local(text: &str) -> (&str, &str) {
    text.rsplit_once('\n').unwrap_or(("", text))
}
```

**Removed fields:**
- `raw_mode_active`
- `key_watcher`
- `buffer_rows`
- `columns`

**Removed imports:**
- `crossterm` cursor/terminal modules

### Key Insight

`MarkdownRender::render()` joins rendered lines with `\n` but adds NO trailing newline. Must use `println!` (not `print!`) to add the separator between rendered head and next tail chunk.

## Why This Works

1. **Source tracking pattern** mirrors TUI's existing `last_ui_output_source` — proven approach.
2. **Line buffering** avoids raw mode entirely — cooked mode handles SIGINT normally.
3. **State machine** is clear: buffer accumulates until `\n`, flush completed lines, hold tail.
4. **`cleanup()` is idempotent** — safe to call multiple times, always resets to clean state.

## Prevention Strategies

**Test Cases Added (12 total):**

- `split_line_tail_local` correctness (3 cases: multi-line, no-newline, trailing-newline)
- `handle_markdown_chunk` buffer accumulation (4 cases: partial, newline flush, tail preservation, multi-chunk)
- `cleanup()` state reset (buffer cleared, `last_ui_output_source` reset)
- `last_ui_output_source` tracking via `emit()` (source set on first chunk, reset after `TurnEvent::Ended`, not cleared between same-source chunks)

**Best Practices:**

- Track "last output source" for heading display, reset on turn boundary
- Avoid raw mode when cooked mode suffices — raw mode should be scoped to TUI paths only
- Buffer incomplete lines until newline boundary for clean streaming

**Code Review Checklist:**

- [ ] Does streaming output accumulate correctly across chunks?
- [ ] Is heading printed once per source, not per chunk?
- [ ] Is raw mode avoided in non-TUI paths?
- [ ] Does `cleanup()` reset all state for next turn?

## Related Issues

- **GitHub:** [#410](https://github.com/dobesv/harnx/issues/410) — Source heading repeated before every streaming chunk
- **GitHub:** [#414](https://github.com/dobesv/harnx/issues/414) — Raw mode used in non-TUI terminal, corrupts terminal on crash
- **Related Solution:** [integration-issues/raw-mode-ctrl-c-interrupt-2026-04-30.md](../integration-issues/raw-mode-ctrl-c-interrupt-2026-04-30.md) — Raw-mode Ctrl-C handling (different issue, same subsystem)
