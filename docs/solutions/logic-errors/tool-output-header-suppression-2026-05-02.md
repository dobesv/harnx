---
title: "Tool output header suppression and template-driven display"
date: 2026-05-02
category: "logic-errors"
problem_type: logic_error
component: "harnx-tui, harnx-cli, harnx-acp, harnx-mcp-bash, harnx-mcp-fs, harnx-mcp-time, harnx-mcp-todo"
root_cause: "redundant tool name headers cluttered output when markdown templates already described the action"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - cli
  - templates
  - display
  - enum-driven
  - header-suppression
plan_ref: "issue-396-tool-output-appearance"
---

## Problem

Tool call output displayed redundant headers (TUI: `→ tool_name`, CLI: `[tool] name`) even when `call_template` markdown already provided a concise description. This cluttered the transcript and reduced information density.

## Symptoms

```text
# Before (TUI):
→ Bash
   $ cargo build

# Before (CLI):
[tool] bash_exec
$ cargo build
```

The `→ Bash` / `[tool] bash_exec` header added no value when the template `$ cargo build` already clearly identified the action.

## Investigation Steps

1. Traced `render_tool_call()` in `harnx-tui/src/render.rs` — found it always emitted header line followed by body
2. Identified `ToolCallBody` enum variants: `Markdown` (template-rendered) vs `Yaml` (raw args)
3. Analyzed CLI `print_tool_started()` in `harnx/src/cli_event_sink.rs` — always prefixed with `[tool] name`
4. Recognized pattern: presence of markdown template determines whether header is useful
5. Designed enum-driven suppression: `ToolCallBody::Markdown` suppresses header, `Yaml`/`None` keeps it

## Root Cause

Display logic did not distinguish between template-rendered markdown (self-describing) and raw YAML args (needs header context). Both paths emitted the same header regardless of whether it added information.

## Solution

### TUI: Enum-driven header suppression in `render_tool_call()`

```rust
fn render_tool_call(tool_name: &str, body: Option<&ToolCallBody>) -> Vec<Line<'static>> {
    let mut lines = match body {
        Some(ToolCallBody::Markdown(_)) => Vec::new(),  // No header
        _ => {
            let header_text = format!("→ {tool_name}");
            Self::render_text_entry("", &header_text, dim_gray, false)
        }
    };
    // ... body rendering
}
```

### TUI: Metadata relocation to separate line

Sequence numbers and timestamps moved from inline suffix to standalone dim line above tool body:

```rust
TranscriptItem::ToolCall { tool_name, body, seq, timestamp } => {
    let mut lines = vec![];
    if let Some(suffix) = Self::render_meta_suffix(*seq, *timestamp, show_seq, show_ts, use_utc) {
        lines.push(Line::from(suffix));  // Meta line FIRST
    }
    lines.extend(Self::render_tool_call(tool_name, body.as_ref()));  // Body AFTER
    lines
}
```

Result: 2-space indent for meta line (`[3] 14:23:01`), 3-space indent for tool body.

### CLI: Extract `format_tool_started()` helper for testability

```rust
fn print_tool_started(&mut self, name: &str, markdown: Option<&str>) {
    let rendered = Self::format_tool_started(name, markdown, |text| {
        self.render_markdown_line(text)
    });
    eprintln!("{rendered}");
}

fn format_tool_started(
    name: &str,
    markdown: Option<&str>,
    mut render_markdown: impl FnMut(&str) -> String,
) -> String {
    match markdown.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => render_markdown(t),  // Just markdown, no prefix
        None => dimmed_text(&format!("[tool] {name}")),  // Fallback to header
    }
}
```

Extraction enables unit testing without stderr capture — pass a closure instead of writing to stdout/stderr.

### Template policy: ASCII icons, no emoji/bold

All tool declarations updated with terse ASCII icon prefixes:

| Icon | Tool category | Examples |
|------|---------------|----------|
| `$` | Execution | `` `$ {{ args.command }}` `` |
| `>` | Spawn | `` `> {{ args.command }}` `` |
| `#` | Read | `# {{ args.path }}` |
| `+` | Create | `+ plan {{ args.name }}` |
| `*` | Edit | `* todo {{ args.id }}` |
| `?` | Search | `? {{ args.pattern }}` |
| `@` | Agent | `@ {{ server_name }} prompt` |
| `-` | Delete | `- todo {{ args.id }}` |
| `>>` | Append | `>> todo {{ args.id }}` |

### ACP dynamic templates via `format!()`

`generate_acp_tools(server_name)` builds templates programmatically:

```rust
fn generate_acp_tools(server_name: &str) -> Vec<ToolDeclaration> {
    let session_new_call_template = format!("@ {} new session", server_name);
    let session_prompt_call_template = format!("@ {} {{{{ args.message | truncate(60) }}}}", server_name);
    // ...
}
```

Server name interpolated at tool generation time; result is valid MiniJinja template.

## Why This Works

**Enum-driven suppression**: `ToolCallBody::Markdown` indicates the template already describes the action — header would be redundant. `ToolCallBody::Yaml` shows raw arguments — header provides essential context.

**Helper extraction**: Separating formatting from I/O enables direct unit testing. Tests call `format_tool_started()` with a passthrough closure and assert on the returned string.

**Metadata separation**: Moving seq/timestamp to a pre-line decouples metadata display from header logic. Visual hierarchy: meta (dim, 2-space) → body (3-space).

**Icon consistency**: ASCII prefixes provide visual scanning cues without emoji/bloat. Backticks highlight primary arguments (command, path).

## Prevention Strategies

**Test Cases:**
- `render_tool_call_markdown_body_suppresses_header` — assert no `→` when markdown present
- `render_tool_call_yaml_body_keeps_header` — assert header present for YAML
- `render_tool_call_meta_line_precedes_markdown_body` — assert `[seq] timestamp` on separate line before body
- `print_tool_started_with_markdown_omits_tool_prefix` — CLI: no `[tool]` with markdown
- `print_tool_started_without_markdown_shows_tool_prefix` — CLI: `[tool]` fallback

**Code Review Checklist:**
- [ ] New MCP tools include `call_template` with ASCII icon prefix
- [ ] Template uses backticks for primary argument (command, path, query)
- [ ] No emoji or bold markdown in templates
- [ ] `ToolCallBody::Markdown` path suppresses header in both TUI and CLI

**Best Practices:**
- Use `ToolCallBody` enum variant to drive display decisions — one source of truth
- Extract formatting helpers for testability — avoid I/O in pure logic
- Keep metadata (seq/timestamp) separate from content for flexible layout

## Related Issues

- **GitHub:** [issue #396](https://github.com/dobesv/harnx/issues/396) — Improve tool output appearance
- **Related Solution:** [mcp-tool-template-acp-propagation-2026-04-30.md](../integration-issues/mcp-tool-template-acp-propagation-2026-04-30.md) — Template rendering and ACP propagation
