---
title: "MCP bash tool output markdown rendering in kagent"
date: 2026-05-31
category: "logic-errors"
problem_type: logic_error
component: "harnx-mcp-bash"
root_cause: "hand-rolled key:value output with ad-hoc markers did not render correctly as markdown"
resolution_type: code_fix
severity: medium
tags:
  - markdown
  - yaml
  - serde_yaml
  - mcp-tool-output
  - html-comments
plan_ref: "harnx-mcp-bash-markdown-output"
---

## Problem

The `harnx-mcp-bash` exec tool output used hand-rolled `key: value` lines and `===== stdout =====` markers that rendered as unstyled text walls in kagent's markdown UI. Commands with colons, quotes, or newlines broke formatting due to lack of proper escaping.

## Symptoms

- Metadata rendered as plain `key: value` text instead of styled YAML block
- Stream content had no code fencing — appeared as unformatted text
- Commands like `echo "foo: bar"` broke YAML-parsing consumers due to unescaped colon
- Empty streams emitted `(empty)` sentinel text, creating special-case handling

## Investigation Steps

1. Reviewed kagent markdown rendering behavior — it interprets MCP tool output as markdown
2. Tested `serde_yaml::to_string` for metadata serialization — correctly handles quoting, multiline block scalars, special characters
3. Evaluated options for stream block delimiters:
   - Plain text markers: `===== stdout =====` — renders as plain text, no structure
   - Backtick fences: ` ``` ` — renders as code but breaks if content contains triple-backticks
   - HTML comments: `<!-- start stdout -->` — invisible in rendered markdown, survives nested backticks
4. Decided on hybrid: HTML comment markers for structural fallback + backtick fences for visual rendering
5. Found trailing-newline defect during review: `build_terminate_result` appended plaintext directly after fenced block output, mashing `signal: SIGTERM` onto the closing fence line

## Root Cause

Output formatting relied on manual string concatenation with ad-hoc marker syntax. No escaping for special characters. No consistent trailing-newline guarantee across code paths. Empty streams had a special sentinel branch instead of uniform treatment.

## Solution

### Metadata Header via serde_yaml

```rust
#[derive(serde::Serialize)]
pub(crate) struct MetadataHeader<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) execution_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<&'a str>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_path"
    )]
    pub(crate) working_dir: Option<&'a Path>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_path"
    )]
    pub(crate) stdout_log_path: Option<&'a Path>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_path"
    )]
    pub(crate) stderr_log_path: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_bytes: Option<usize>,
}

fn serialize_optional_path<S>(path: &Option<&Path>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match path {
        Some(path) => serializer.serialize_str(&path.display().to_string()),
        None => serializer.serialize_none(),
    }
}

pub(crate) fn render_metadata_header(output: &mut String, metadata: MetadataHeader<'_>) {
    let yaml = serde_yaml::to_string(&metadata).expect("MetadataHeader should serialize to YAML");
    let _ = write!(output, "```yaml\n{yaml}```");
}
```

### Stream Blocks with HTML Comment Markers

```rust
pub(crate) fn render_stream_block(
    name: &str,
    content: &str,
    truncate_opts: &TruncateOpts,
    log_hint: Option<(&str, &Path)>,
) -> String {
    let sanitized = sanitize_output_text(content);
    let truncated = truncate_output(&sanitized, truncate_opts);
    let mut block = format!("<!-- start {name} -->\n```\n{truncated}");
    // truncation hint appended inside fence if needed...
    block.push_str("\n```\n<!-- end {name} -->");
    block
}
```

**New output shape:**

~~~text
```yaml
execution_id: exec-...
status: exited
exit_code: 0
command: echo test
```

<!-- start stdout -->
```
test
```
<!-- end stdout -->

<!-- start stderr -->
```
```
<!-- end stderr -->
~~~

### Trailing Newline Guarantee

Centralized in `build_success_result`:

```rust
fn build_success_result(output: String, summary: String, annotations: Vec<Annotation>) -> CallToolResult {
    // Ensure trailing newline for assistant-audience output
    let output = if output.ends_with('\n') { output } else { format!("{output}\n") };
    CallToolResult::success(vec![
        Content::text(output).with_annotations(annotations),
        Content::text(summary).with_audience(["user"]),
    ])
}
```

### Terminate Path Fix

**Before (broken):**
```rust
let mut output = String::new();
render_metadata_header(&mut output, metadata);
write!(output, "signal: {signal}")?;  // Mashes onto closing fence line
```

**After (fixed):**
```rust
let mut output = String::new();
render_metadata_header(&mut output, metadata);
write!(output, "\n\nsignal: {signal}")?;  // Blank line separation
build_success_result(output, summary, vec![])
```

## Why This Works

1. **serde_yaml** handles YAML spec correctly — block scalars for multiline commands, proper quoting for colons/quotes, no `null` noise via `skip_serializing_if`.
2. **HTML comment markers** are invisible in rendered markdown but survive even if stream content contains triple-backticks. LLM and downstream consumers can still locate block boundaries.
3. **Empty streams** emit the same structure (start marker + empty fence + end marker) — no special-case branches, no `(empty)` sentinel.
4. **Centralized trailing newline** ensures all assistant-audience code paths end cleanly.

## Prevention Strategies

**Test Cases:**
- Assert metadata renders as valid YAML (`serde_yaml::from_str` roundtrips)
- Assert commands with colons/quotes/newlines serialize correctly
- Assert empty streams produce both start and end markers
- Assert assistant output ends with `\n`

**Code Review Checklist:**
- [ ] When appending plaintext after a fenced block, insert `\n\n` separation
- [ ] All paths building `CallToolResult` for assistant audience should go through `build_success_result`
- [ ] `serde::Serialize` structs with `Option` fields should use `skip_serializing_if = "Option::is_none"`

**Known Limitation:**
- Triple-backticks in stream content break the plain fence rendering — accepted as low-probability edge case. HTML comment markers preserve structure for programmatic consumers.

## Related Issues

- **GitHub:** [#713](https://github.com/dobesv/harnx/issues/713) — Improve bash exec output markdown formatting
- **Plan:** `harnx-mcp-bash-markdown-output`
