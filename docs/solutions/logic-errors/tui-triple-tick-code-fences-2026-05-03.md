---
title: "TUI code fence rendering for multi-line bash commands"
date: 2026-05-03
category: "logic-errors"
problem_type: logic_error
component: "harnx-tui"
root_cause: "inline markdown renderer couldn't handle block-level code fences"
resolution_type: code_fix
severity: medium
tags:
  - markdown
  - code-fences
  - tui
  - rendering
  - templates
plan_ref: "bash-triple-tick-434"
---

## Problem

Bash tool `call_template` used single backticks for command display, causing multi-line commands to render as inline code on a single line in the TUI. The 3-space indentation applied to tool call bodies also interfered with block-level markdown parsing.

## Symptoms

- Multi-line bash commands displayed as single run-on line: `$ echo 'a\nb\nc'` instead of properly formatted blocks
- Single backticks rendered literally in some contexts instead of creating code formatting
- Tool call bodies had fixed 3-space indentation regardless of content type

## Investigation Steps

1. Traced rendering path: `ToolCallBody::Markdown` passed through `render_indented_markdown_line` which used `tui_markdown` on each line individually
2. Identified that block-level constructs (code fences, lists, blockquotes) require whole-document parsing, not line-by-line
3. Found `tui_markdown::from_str` emits fence marker lines (`` ```sh ``, `` ``` ``) as unstyled single-span lines that leaked into output
4. Tested triple-tick templates with multi-line commands — worked in block mode but showed literal fence markers

## Root Cause

Two issues compounded:

1. **Inline renderer limitation**: The old `render_indented_markdown_line` function parsed each line independently with `tui_markdown::from_str`. This works for inline emphasis (`**bold**`, `` `code` ``) but cannot handle block-level markdown like fenced code blocks which span multiple lines.

2. **Fence marker pass-through**: `tui_markdown::from_str` emits opening and closing fence lines as unstyled `Line` objects with a single span containing the fence text. These passed through to the TUI as visible `` ```sh `` and `` ``` `` lines.

## Solution

### 1. Switch to block renderer

Replaced line-by-line inline rendering with `render_markdown_block` that parses the entire text as a document:

```rust
// Before: line-by-line inline parsing
fn render_indented_markdown_line(text: &str, base_style: Style) -> Line<'static> {
    let parsed = tui_markdown::from_str(text, None);
    // ...process single line...
}

// After: whole-document block parsing
fn render_markdown_block(text: &str) -> Vec<Line<'static>> {
    let body_base = Style::default().add_modifier(Modifier::DIM);
    crate::render_helpers::markdown_lines(text, body_base)
}
```

### 2. Filter fence marker lines

Added `is_fence_marker_line` to detect and remove fence markers from `tui_markdown` output:

```rust
fn is_fence_marker_line(line: &ratatui::text::Line<'_>) -> bool {
    if line.spans.len() != 1 {
        return false;
    }
    let span = &line.spans[0];
    // Must be unstyled (no fg/bg override set by tui-markdown).
    if span.style.fg.is_some() || span.style.bg.is_some() {
        return false;
    }
    let content = span.content.trim();
    // Match ``` optionally followed by an ASCII-lowercase language hint.
    content.starts_with("```") && content[3..].chars().all(|c| c.is_ascii_lowercase())
}

pub(crate) fn markdown_lines(text: &str, base_style: Style) -> Vec<Line<'static>> {
    tui_markdown::from_str(text, None)
        .lines
        .into_iter()
        .filter(|line| !is_fence_marker_line(line))
        .map(|line| { /* patch base_style */ })
        .collect()
}
```

### 3. Update templates with proper fence structure

Changed bash tool templates to use triple-tick fences. Critical: closing fence must be on its own line when optional metadata follows:

```rust
// exec tool template
"```sh\n$ {{ args.command }}\n```{% if args.working_dir or args.timeout_secs %}\n{% if args.working_dir %}({{ args.working_dir }}) {% endif %}{% if args.timeout_secs %}[{{ args.timeout_secs }}s]{% endif %}{% endif %}"

// spawn tool template
"```sh\n> {{ args.command }}\n```{% if args.working_dir %}\n({{ args.working_dir }}){% endif %}"
```

The newline before `{% if args.working_dir %}` ensures the closing ` ``` ` is isolated. Without it, metadata would render on the fence line (`` ``` [10s] ``), breaking the filter.

### 4. Remove 3-space indentation

Removed the fixed indentation prefix from `render_tool_call` and `render_tool_result_markdown`. Block-level markdown now renders at the same column as other transcript content.

### 5. Gate test-only functions with `#[cfg(test)]`

Functions like `markdown_line_spans` used only in tests caused dead-code warnings:

```rust
#[cfg(test)]
pub(crate) fn markdown_line_spans(text: &str, base_style: Style) -> Line<'static> {
    // ...
}
```

## Why This Works

**Block parsing**: `tui_markdown::from_str` on the full text lets the underlying `pulldown-cmark` parser recognize block boundaries correctly. Code fences become proper code blocks with syntax highlighting, not inline text.

**Fence filtering**: `tui_markdown` emits fence markers as unstyled single-span lines. Code body lines get syntax highlighting (styled spans), so they pass the `fg.is_some()` check. The filter removes only the structural markers.

**Template structure**: Putting the closing fence on its own line means `is_fence_marker_line` sees pure `` ``` `` (content = "```"), which matches the filter. Metadata lines appear after the fence and render as inline text.

**Conservative filter design**: The filter fails safe — false negatives (leaving fence markers visible) are cosmetic, false positives (dropping content lines) would lose user data. The strict single-span, unstyled, lowercase-hint checks ensure only genuine fence markers are removed.

## Prevention Strategies

**Test Cases:**
- Add tests for multi-line commands in code fences
- Test `timeout_secs` without `working_dir` template branch
- Test metadata rendering after fenced code block

**Template Design Checklist:**
- [ ] Closing fence always on its own line when metadata follows
- [ ] Use `\n{% if %}` not `{% if %}` on same line as fence
- [ ] Test both branches of conditional metadata

**Code Review Checklist:**
- [ ] Test-only functions marked `#[cfg(test)]`
- [ ] Block-level markdown uses `markdown_lines`, not line-by-line parsing
- [ ] Fence marker filter handles language hints used in templates

## Related Issues

- **Issue:** [#434](https://github.com/example/repo/issues/434) — Multi-line bash commands render incorrectly
- **Related Solution:** [logic-errors/unified-tool-metadata-rendering-2026-04-30.md](../logic-errors/unified-tool-metadata-rendering-2026-04-30.md) — Tool response metadata consistency
