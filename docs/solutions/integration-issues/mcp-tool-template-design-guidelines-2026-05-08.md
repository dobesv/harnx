---
title: "MCP tool call_template design guidelines"
date: 2026-05-08
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp-servers"
root_cause: "inconsistent tool template design caused terse output and hidden parameters"
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - templates
  - minijinja
  - tui
  - tool-display
plan_ref: "issue-498-tool-templates-more-info"
---

> **Where the template goes.** The `.with_meta(...)` snippets below predate the
> NATS toolsets. For a first-party server, the template belongs on the
> `ToolSpec` the `Toolset` advertises — use `ToolSpec::with_call_template` /
> `with_result_template`. That is what the runtime reads. Attaching it only to
> the rmcp `ServerHandler` is how the templates silently stopped rendering. Keep
> the strings in a shared `tool_templates` module so the handler and the toolset
> can't drift. The notation and filter guidance in this doc still applies.

## Problem

MCP tool call templates were too terse, showing minimal parameter info with opaque single-character prefixes (`#`, `?`, `~`, `+`). Users couldn't understand what a tool call was doing without expanding to full args. Large text inputs (plan body, note body) were invisible in the transcript.

## Symptoms

```text
note harnx-497/findings-Calliope  — no icon context, what tool is this?
~ task harnx-497/review-terpsichore — `~` is opaque
# AGENTS.md — what does `#` mean? (read tool)
? crates — what does `?` mean? (ls tool)
? /impl AgentEventSink/ crates/harnx/src/cli_event_sink.rs — grep with no icon
```

Large text params (plan body, task body, grep pattern) weren't shown. Optional params like `assignee`, `executor`, `tags` were missing entirely.

## Investigation Steps

1. Audited existing templates across all MCP servers (fs, bash, plans, time)
2. Identified single-char prefixes that lacked semantic meaning
3. Found text body params were omitted entirely from templates
4. Discovered MiniJinja `truncate()` filter was available but unused
5. Checked actual `*Params` struct field names — found naming inconsistencies:
   - `AddPlanParams.body` vs `UpdatePlanParams.content` (different field names)
   - `AppendTaskParams.text` not `body` (wrong assumption)
6. Tested multi-line templates with `\n` — worked correctly
7. Verified code fence constraint from issue #434: closing ` ``` ` must be isolated

## Root Cause

**Inconsistent template design**: Each server used different conventions for icons, parameter ordering, and text truncation. No documented guidelines existed.

**Silent failures**: MiniJinja's Lenient undefined behavior returns empty string for missing fields. Wrong field names in templates produce no output with no error.

**Fence marker constraint**: The `exec`/`spawn` tools use code fence templates. The TUI's `is_fence_marker_line()` filter requires the closing ` ``` ` on its own line. Annotations after the fence must be inside the `{% if %}` block, after a newline.

## Solution

### 1. Full Param Coverage Principle

Every optional parameter a tool accepts should appear in its `call_template`, even if highly condensed. Use compact notations:

| Parameter Type | Notation | Example |
|---|---|---|
| Line limits | `[head:N]`, `[tail:N]` | `[head:20]` |
| Byte limits | `[:Nb]` | `[:1Kb]` |
| Max results | `[max:N]` | `[max:50]` |
| Flag (boolean) | `i` (for ignore_case) | `i` |
| Tags | `#tag` | `# urgent` |
| Assignee | `@assignee` | `@alice` |
| Executor | `▶executor` | `▶hephaestus` |
| Task count | `[N tasks]` | `[5 tasks]` |
| Channel/timeout | `(Nch)`, `[Ns]` | `(5ch)`, `[30s]` |

### 2. Multi-line Templates

Templates can include `\n` to show content on a second line. Use for large text inputs:

```rust
.with_meta(Meta(json!({
    "call_template": "➕ plan {{ args.name }}{% if args.body %}\n{{ args.body | truncate(80) }}{% endif %}"
}).as_object().unwrap().clone()))
```

Result in TUI:
```text
➕ plan my-plan
First 80 chars of body content...
```

### 3. MiniJinja Filters

- `truncate(N)` — truncate to N chars with ellipsis (`...`)
- `truncate(N, end='')` — truncate without ellipsis
- `length` — char count: `{{ args.content | length }}`
- `join(' #')` — join array with prefix: `#tag1 #tag2 #tag3`
- `default('')` — fallback for missing values

### 4. Code Fence Constraint (Bash exec/spawn)

The closing fence must appear on its own line. Annotations go on the line *after* the fence:

```rust
// CORRECT: newline before {% if %}
"```sh\n$ {{ args.command }}\n```{% if args.working_dir %}\n({{ args.working_dir }}){% endif %}"

// WRONG: metadata on fence line breaks is_fence_marker_line filter
"```sh\n$ {{ args.command }}\n```[10s]"  // Fence line is "```[10s]", not pure "```"
```

Template is hardcoded in TWO places:
- `harnx-mcp-bash/src/server.rs` — actual server
- `harnx-core/src/tool.rs` — test helper

Update both together.

### 5. Param Name Verification

Always check the actual `*Params` struct fields before writing templates:

```rust
// crates/harnx-mcp-plans/src/server.rs
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct AddPlanParams {
    pub name: String,
    pub body: Option<String>,      // ← "body" field
}

pub struct UpdatePlanParams {
    pub name: String,
    pub content: Option<String>,   // ← "content" field (different!)
}

pub struct AppendTaskParams {
    pub plan: String,
    pub id: String,
    pub text: Option<String>,      // ← "text" field, not "body"
}
```

Wrong field names silently produce empty output (MiniJinja Lenient mode).


Show full delegation message on its own line — no truncation. The full prompt is valuable context:

```rust
let session_prompt_call_template = format!("@ {}\n{{{{ args.message }}}}", server_name);
```

Result:
```text
@ playwright
Click the submit button and wait for navigation...
```

### 7. Unicode Icons

Unicode icons are acceptable (Issue #498 explicitly allows):

| Icon | Tool |
|---|---|
| 📖 | read |
| ✏️ | write |
| ✂️ | edit (when deleting) |
| 🔧 | edit (when modifying) |
| 📂 | ls |
| 🔍 | grep |
| 🔎 | find |
| ⏪ | rollback |
| 📋 | read plan/task/note |
| ➕ | create/add |
| 🔄 | update |
| 🗑️ | delete |
| 📝 | note/task append |
| ⏱️ | time/wait |

## Why This Works

**Visual scanning**: Unicode icons + full param coverage let users understand tool calls at a glance without expanding to full args.

**Context preservation**: Truncated body content on a second line preserves the "what" while keeping templates compact (≤2 lines typical).

**Silent failure prevention**: Verifying param names against actual structs prevents the empty-output bug that Lenient MiniJinja would hide.

**TUI compatibility**: Code fence structure ensures the `is_fence_marker_line()` filter works correctly for bash tool templates.

## Prevention Strategies

**Template Review Checklist:**
- [ ] All optional params appear in template (use compact notation)
- [ ] Large text inputs shown with `\n` + `truncate(80)`
- [ ] Field names verified against actual `*Params` struct
- [ ] Code fence closing ` ``` ` on its own line when metadata follows
- [ ] Unicode icon prefix for visual identification
- [ ] `{% if %}` guards for optional params
- [ ] `default('')` used when missing value should render as empty

**Test Cases:**
- Add snapshot tests for tools with text body params
- Test templates with and without optional params
- Verify no empty output from wrong field names (would be a bug)

**Before/After Verification:**
- Run `cargo insta test` after template changes
- Review `.snap.new` files for correctness
- Accept with `mv *.snap.new *.snap`

## Related Issues

- **Issue:** #498 — MCP tool templates less terse, better context
- **Issue:** #434 — Multi-line bash commands rendering (code fence constraint)
- **Related Solution:** [logic-errors/tui-triple-tick-code-fences-2026-05-03.md](../logic-errors/tui-triple-tick-code-fences-2026-05-03.md) — Code fence rendering
- **Related Solution:** [logic-errors/minijinja-system-prompt-templating-2026-04-25.md](../logic-errors/minijinja-system-prompt-templating-2026-04-25.md) — MiniJinja fundamentals
