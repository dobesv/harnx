---
title: "MCP tool template rendering and ACP sub-agent event propagation"
date: 2026-04-30
category: "integration-issues"
problem_type: integration_issue
component: "harnx-acp, harnx-acp-server, harnx-tui, harnx-runtime"
root_cause: "MCP tool call/result templates not rendered for restored sessions or sub-agent events; lifecycle events dropped by ACP layer"
resolution_type: code_fix
severity: high
tags:
  - mcp
  - templating
  - acp
  - sub-agent
  - session-restore
  - event-propagation
plan_ref: "fix-mcp-template-rendering-385-387"
---

## Problem

MCP tool `call_template` and `result_template` directives were not rendered in two contexts:

1. **Session restore (#385)**: Restored sessions displayed raw JSON or YAML fallback instead of rendered MiniJinja templates for MCP tool calls/results
2. **Sub-agent events (#387)**: Parent TUI showed raw tool names for sub-agent MCP tool calls; templates not propagated through ACP layer

Additionally, `ToolEvent::Update` and `ToolEvent::Completed` were silently dropped by `AcpChunkSink`, breaking sub-agent lifecycle visibility in parent TUI.

## Symptoms

```text
#385 - Session Restore:
- Restored transcript showed: {"command": "ls -la"} instead of **$** `ls -la`
- Tool results displayed raw JSON instead of template-rendered markdown
- Only affected MCP tools with call_template/result_template configured

#387 - Sub-Agent Events:
- Parent TUI showed tool name "bash_exec" without markdown formatting
- Sub-agent tool events appeared as raw tool calls, no rich display
- ToolEvent::Update/Completed never reached parent TUI from sub-agent sessions

ACP Lifecycle:
- ToolEvent::Update lost (progress indicators missing)
- ToolEvent::Completed lost (completion status never shown)
```

## Investigation Steps

1. Traced `ToolEvent::title` field usage across codebase — found semantic confusion: field carried pre-rendered MiniJinja markdown for display, not a UI title label
2. Identified `messages_to_transcript_items()` in `lifecycle.rs` had no access to tool declarations, couldn't render templates during session restore
3. Found `render_call_for_display` and `render_result_for_display` were private to `harnx-runtime`, unavailable to TUI layer
4. Discovered `AcpChunkSink::emit()` matched only `ToolEvent::Started`, dropping `Update`/`Completed` variants
5. Traced ACP `ToolCall` struct — `title` field used for tool name, no field for markdown payload
6. Noted ACP `ToolCallUpdate.fields.title` carried rendered display text but client didn't extract it

## Root Cause

**Naming confusion**: `ToolEvent::title` field name suggested it was a label, but it carried rendered MiniJinja markdown. This made the field's purpose unclear to consumers.

**Missing template access in session restore**: `messages_to_transcript_items()` converted stored messages to transcript items without access to `ToolDeclaration` map. Without decls, couldn't render `call_template`/`result_template`. Render helpers existed but were private.

**ACP event gap**: `AcpChunkSink` matched `ToolEvent::Started` but had no arms for `Update`/`Completed`. These events were silently dropped. The `AcpForward` enum only had `ToolCall` variant, no variants for update/completion.

**ACP markdown not propagated**: Server captured `ToolEvent::Started.markdown` but didn't embed it in ACP protocol. Client received `ToolCall` with no markdown field, defaulted to `None`.

## Solution

### 1. Rename `ToolEvent::title` → `markdown`

Semantic rename across 10+ files to clarify field carries pre-rendered markdown:

```rust
// crates/harnx-core/src/event.rs
pub enum ToolEvent {
    Started {
        id: String,
        name: String,
        kind: ToolKind,
        markdown: Option<String>,  // was: title
        input: serde_json::Value,
        locations: Vec<ToolLocation>,
    },
    Update {
        id: String,
        markdown: Option<String>,  // was: title
        status: Option<ToolStatus>,
        content: Option<Vec<ContentBlock>>,
    },
    Completed {
        id: String,
        output: serde_json::Value,
        markdown: Option<String>,  // was: title
    },
    // ...
}
```

### 2. Expose render helpers publicly

```rust
// crates/harnx-runtime/src/tool.rs

/// Render tool call via call_template, returns None if no template.
pub fn render_call_for_display(
    call: &ToolCall,
    input: &serde_json::Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    render_call(call, input, raw_fallback, decl_map)
}

/// Render tool result via result_template, returns None if no template.
pub fn render_result_for_display(
    call: &ToolCall,
    result: &serde_json::Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    render_result(call, result, raw_fallback, decl_map)
}
```

### 3. Session restore: pass decl_map to transcript conversion

```rust
// crates/harnx-tui/src/lifecycle.rs

pub(crate) fn messages_to_transcript_items(
    messages: &[Message],
    decl_map: &HashMap<String, ToolDeclaration>,  // NEW PARAM
) -> Vec<TranscriptItem> {
    // For each tool call, attempt template render:
    if let Some(decl) = decl_map.get(&tool_call.name) {
        if let Some(rendered) = render_call_for_display(
            &tool_call,
            &input,
            &raw_yaml,
            decl_map,
        ) {
            body = Some(ToolCallBody::Markdown(rendered));
        }
    }
    // ...
}

pub(crate) fn session_history_transcript_items(
    cfg: &GlobalConfig,
    // ...
) -> Vec<TranscriptItem> {
    let decl_map = cfg.tool_declarations_for_use_tools(Some("*"))
        .into_iter()
        .map(|d| (d.name.clone(), d))
        .collect();
    messages_to_transcript_items(&messages, &decl_map)
}
```

### 4. ACP: Add markdown propagation via `harnx:markdown` meta key

Server embeds markdown in ACP ToolCall meta:

```rust
// crates/harnx-acp-server/src/lib.rs

fn spawn_notify_tool_call(
    conn: &Option<Rc<acp::AgentSideConnection>>,
    session_key: &str,
    id: String,
    name: String,
    input: serde_json::Value,
    markdown: Option<String>,  // NEW PARAM
    source: Option<AgentSource>,
) {
    let mut meta_map: Option<serde_json::Map<String, serde_json::Value>> = None;
    if let Some(source) = source.as_ref() {
        meta_map = meta_from_source(source);
    }
    if let Some(md) = markdown.filter(|t| !t.is_empty()) {
        let map = meta_map.get_or_insert_with(serde_json::Map::new);
        map.insert("harnx:markdown".to_string(), serde_json::Value::String(md));
    }
    if let Some(map) = meta_map {
        tc = tc.meta(map);
    }
    // ...
}
```

Client extracts markdown from meta:

```rust
// crates/harnx-acp/src/client.rs

fn markdown_from_meta_value(value: &serde_json::Value) -> Option<String> {
    value
        .get("harnx:markdown")
        .and_then(serde_json::Value::as_str)
        .filter(|t| !t.is_empty())
        .map(ToOwned::to_owned)
}

// In SessionUpdate::ToolCall handler:
let meta = tc.meta.as_ref().map(|m| serde_json::json!(m));
let markdown = meta.as_ref().and_then(markdown_from_meta_value);
```

### 5. ACP: Forward Update/Completed events

Added variants to `AcpForward` enum:

```rust
// crates/harnx-acp-server/src/lib.rs

enum AcpForward {
    ToolCall { id, name, input, markdown, source },
    ToolUpdate { id, markdown, status, source },      // NEW
    ToolCompleted { id, output, markdown, source },   // NEW
}

impl AgentEventSink for AcpChunkSink {
    fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
        match event {
            AgentEvent::Tool(ToolEvent::Started { markdown, .. }) => {
                self.tx.send(AcpForward::ToolCall { markdown, .. })
            }
            AgentEvent::Tool(ToolEvent::Update { markdown, status, .. }) => {
                self.tx.send(AcpForward::ToolUpdate { markdown, status, .. })
            }
            AgentEvent::Tool(ToolEvent::Completed { markdown, output, .. }) => {
                self.tx.send(AcpForward::ToolCompleted { markdown, output, .. })
            }
            // ...
        }
    }
}
```

Client decodes `ToolCallUpdate` with status:

```rust
// crates/harnx-acp/src/client.rs

acp::SessionUpdate::ToolCallUpdate(tcu) => {
    let markdown = tcu.fields.title.clone();
    let status = tcu.fields.status;
    match status {
        Some(acp::ToolCallStatus::Completed) if tcu.raw_output.is_some() => {
            // Completed with output -> ToolEvent::Completed
            Some(AgentEvent::Tool(ToolEvent::Completed {
                id: tcu.tool_call_id.to_string(),
                output: tcu.raw_output.unwrap(),
                markdown,
            }))
        }
        _ => {
            // In-progress or pending -> ToolEvent::Update
            Some(AgentEvent::Tool(ToolEvent::Update {
                id: tcu.tool_call_id.to_string(),
                markdown,
                status: status.map(|s| match s {
                    acp::ToolCallStatus::InProgress => ToolStatus::InProgress,
                    acp::ToolCallStatus::Completed => ToolStatus::Completed,
                    // ...
                }),
                content: None,
            }))
        }
    }
}
```

## Why This Works

**Semantic clarity**: Renaming `title` → `markdown` makes the field's purpose explicit. Developers immediately understand it carries rendered display text, not a label.

**Session restore renders templates**: By building a `decl_map` from `GlobalConfig` and passing it through `messages_to_transcript_items`, the TUI layer can call the now-public render helpers to apply templates to restored tool calls/results.

**ACP markdown propagation**: Embedding rendered markdown in the `harnx:markdown` meta key keeps it separate from ACP protocol's `title` field (which carries the tool name). The namespaced key avoids collisions with standard ACP fields.

**Event lifecycle restored**: `ToolUpdate` and `ToolCompleted` variants in `AcpForward` ensure all tool lifecycle events flow through the ACP layer. Client decoding maps ACP `ToolCallUpdate` status back to appropriate `ToolEvent` variants.

**Protocol compatibility**: Using ACP's existing `meta` field for markdown avoids protocol changes. The `ToolCallUpdate.fields.title` field already carries display text for updates, repurposed for our markdown.

## Prevention Strategies

**Test cases:**

- Session restore: `messages_to_transcript_items` with decl_map renders `ToolCallBody::Markdown` when template exists
- Session restore: Falls back to YAML `ToolCallBody::Yaml` when no template configured
- ACP round-trip: `harnx:markdown` in ToolCall meta extracts correctly on client
- ACP: `ToolEvent::Update` forwards as `ToolCallUpdate` with `status=InProgress`
- ACP: `ToolEvent::Completed` forwards as `ToolCallUpdate` with `status=Completed` + `raw_output`
- ACP: `forward_acp_chunks` preserves nested `ToolEvent::Completed/Update` with `AgentSource`

**Code review checklist:**

- [ ] `ToolEvent` field names reflect actual content/purpose
- [ ] New event propagation paths have tests for all variants
- [ ] ACP meta keys are namespaced (`harnx:` prefix)
- [ ] Session restore considers template rendering requirements
- [ ] Public render helpers documented with `///` doc comments

**Monitoring:**

- Track `ToolEvent::Completed` delivery rate in ACP layer
- Log dropped/duplicate events in `AcpChunkSink`

## Related Issues

- **Issue:** #385 — Session restore template rendering
- **Issue:** #387 — ACP sub-agent markdown propagation
- **Related Solution:** [logic-errors/minijinja-system-prompt-templating-2026-04-25.md](../logic-errors/minijinja-system-prompt-templating-2026-04-25.md) — MiniJinja context for system prompts
- **Test coverage:** Existing `repro_249` tmux e2e covers sub-agent MCP tool result visibility in parent TUI
