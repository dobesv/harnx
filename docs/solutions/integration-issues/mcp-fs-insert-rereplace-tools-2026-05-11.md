---
title: "MCP-FS insert and re_replace tool implementation"
date: 2026-05-11
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp-fs"
root_cause: "missing insertion and regex replacement tools in filesystem MCP server"
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - filesystem
  - editing
  - regex
  - utf-8
plan_ref: "harnx-511-insert-rereplace"
---

## Problem

harnx-mcp-fs MCP server lacked tools for position-based text insertion and regex-based find/replace. Users had to read files, apply transformations client-side, and write back — inefficient for common editing operations like inserting at a specific line/column or applying regex replacements with capture groups.

## Symptoms

- No MCP tool for inserting text at a specific line/column position
- No MCP tool for regex-based find/replace with capture group backreferences
- Clients forced to read-modify-write for simple edits
- `edit` tool only supports exact text matching, not patterns

## Investigation Steps

1. Reviewed existing `edit` tool — exact text replacement only, no position insertion
2. Identified need for two tools: position-based insert, regex-based replace
3. Analyzed `write_file_impl` and `edit_file_impl` for mutation pattern: `validate_write_path`, history snapshots, size guards
4. Researched `fancy_regex` crate — supports lookahead/lookbehind, retains `fancy_regex::Regex` API compatibility
5. Tested `split_inclusive('\n')` — preserves line endings when splitting content for line-based operations
6. Investigated `is_char_boundary()` — required for safe UTF-8 slicing with byte offsets

## Root Cause

**Missing tools**: No `insert` or `re_replace` tools in FsServer. Mutation tools existed only for full-file write (`write`) and exact-text replacement (`edit`).

**UTF-8 slicing risk**: Column-based insertion uses byte offsets; slicing without boundary validation panics inside multibyte characters.

**Regex error handling**: `fancy_regex::find_iter` returns `Result<Match>` per iteration; naive counting can swallow errors.

## Solution

### insert tool

Position-based insertion with optional column offset:

```rust
#[derive(Debug, Deserialize)]
pub struct InsertParams {
    pub path: String,
    pub insert_line: usize,       // 0 = prepend; 1..=N = 1-based line numbers (N appends)
    pub insert_text: String,
    #[serde(default)]
    pub column: Option<usize>,   // 1-indexed byte offset within line
}
```

**Implementation key points:**

1. Use `validate_write_path` (not `validate_path`) — mutation requires write validation
2. `split_inclusive('\n')` preserves line endings when splitting
3. `insert_line: 0` prepends before first line
4. `insert_line: N` where N = total_lines appends at end
5. Column slicing requires `is_char_boundary()` check:

```rust
let insert_index = column - 1;
if insert_index > stripped_line.len() || !stripped_line.is_char_boundary(insert_index) {
    return tool_error(format!(
        "column {} is not a valid UTF-8 character boundary in line {}",
        column, params.insert_line
    ));
}
let new_line = format!(
    "{}{}{}",
    &stripped_line[..insert_index],
    params.insert_text,
    &stripped_line[insert_index..]
);
```

### re_replace tool

Regex find/replace with fancy_regex:

```rust
#[derive(Debug, Deserialize)]
pub struct ReReplaceParams {
    pub path: String,
    pub pattern: String,         // fancy_regex pattern
    pub replacement: String,     // $0, $1, $2 for groups
    #[serde(default)]
    pub replace_all: Option<bool>,
}
```

**Implementation key points:**

1. `Regex::new()` validates pattern, returns `invalid_params` error on invalid regex
2. Match counting with graceful degradation:

```rust
let count = regex
    .find_iter(&content)
    .filter_map(|result| result.ok())  // best-effort: swallow iterator errors
    .count();
```

3. Error on no match: `"pattern did not match anything in the file"`
4. Error on multiple matches without flag: `"Found N matches; set replace_all=true to replace all occurrences"`
5. Returns match count in success message: `"Replaced N match(es) in <path>"`

### History snapshot pattern

Both tools follow the before/after snapshot pattern from `edit_file_impl`:

```rust
let before_snap = self.history.snapshot_file(&path, "before insert").await
    .map_err(|e| log::warn!("history before-snapshot failed: {e}")).ok();

// ... perform mutation ...

let after_snap_result = if let Some(before) = before_snap {
    match self.history.snapshot_file(&path, "after insert").await {
        Ok(after) => {
            let diff = self.history.diff_commits(&repo_dir, before, after).await.unwrap_or_default();
            Some((after, diff))
        }
        Err(e) => { log::warn!("history after-snapshot failed: {e}"); None }
    }
} else { None };
```

## Why This Works

- **validate_write_path**: Mutation tools require write access validation, not just path resolution
- **split_inclusive('\n')**: Preserves exact line endings (including trailing newline on last line) during split/join
- **is_char_boundary()**: Rust strings are UTF-8; slicing at arbitrary byte offsets inside multibyte characters panics. Must validate boundary before slicing.
- **filter_map(|r| r.ok())**: Matches existing project pattern for graceful degradation when regex iterator encounters runtime errors
- **Error on no match**: Prevents silent no-ops; user explicitly wants replacement to occur
- **Error on ambiguous multi-match**: Forces explicit `replace_all=true` decision, prevents accidental bulk replacement

## Prevention Strategies

**Test Cases:**
- UTF-8 boundary validation: insertion at valid and invalid byte offsets in emoji/CJK lines
- Line boundary edge cases: `insert_line: 0`, `insert_line: total_lines`, `insert_line > total_lines`
- Column boundary: `column: 1` (start), `column: past_end`, `column: inside_multibyte`
- Regex patterns with lookahead/lookbehind
- Capture group backreferences: `$0`, `$1`, `$2`
- No-match error, multi-match error, invalid-regex error
- CRLF line ending preservation in column insertion

**Code Review Checklist:**
- [ ] Mutation tools use `validate_write_path`, not `validate_path`
- [ ] Byte-offset slicing has `is_char_boundary()` guard
- [ ] Regex tools handle iterator errors gracefully
- [ ] Ambiguous operations require explicit flags (e.g., `replace_all`)
- [ ] Size guards on both input and output (`WRITE_MAX_BYTES`)
- [ ] History snapshot pattern consistent with existing mutation tools

**Best Practices:**
- Always check `is_char_boundary()` before slicing `&str` by byte offset
- Prefer `split_inclusive('\n')` for line-based operations to preserve endings
- Return specific errors for no-match and ambiguous-match cases in search/replace tools
- Follow existing mutation-tool patterns for consistency (history, validation, size guards)

## Related Issues

- **Plan:** [harnx-511-insert-rereplace](/plans/harnx-511-insert-rereplace.md)
- **Related Solution:** [mcp-tool-template-design-guidelines-2026-05-08.md](../integration-issues/mcp-tool-template-design-guidelines-2026-05-08.md) — call_template design for tool display
- **Future Work:** Extract shared history snapshot/write/diff helper (~20 lines duplicated per mutation tool)
