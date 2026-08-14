---
title: "AG-UI additive persisted field: the live→persist→restore seam trap"
date: 2026-07-10
category: "integration-issues"
problem_type: integration_issue
component: "harnx-serve, harnx-runtime, harnx-core, web"
root_cause: "Additive field populated only for live event emission, never written to persisted ToolResult before append_session_tool_results"
resolution_type: code_fix
severity: high
tags:
  - ag-ui
  - session-persistence
  - additive-fields
  - serde-default
  - assistant-ui
  - tool-summary
plan_ref: "harnx-webui-tui-parity"
---

# AG-UI additive persisted field: the live→persist→restore seam trap

## Problem

Adding a `markdown: Option<String>` field to `ToolResult` and rendering it into emitted AG-UI events is not sufficient. If the persisted object is not populated before `append_session_tool_results`, restored sessions silently lose the field — tool-call summary cards appear blank after page reload.

## Symptoms

```
- Live sessions: tool summary cards render correctly (markdown from tool_summary custom event)
- Restored sessions: tool summary cards empty/blank
- Session log JSON: "markdown": null for all tool_results entries
- Mock-based tests passed (they hand-set the field)
- Only production flow (execution → persist → restore) revealed the bug
```

## Investigation Steps

1. Added `markdown: Option<String>` to `harnx_core::ToolResult` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
2. Live path: emitted `tool_summary` custom event keyed by `tool_call_id` when `ToolEvent::Started.markdown` present — worked.
3. Restore path: `history_messages_for_snapshot` preferred `.markdown` over `.output` — also correct.
4. Review blocker: persisted sessions still had `markdown: null`. The field was never written to `ToolResult` before `append_session_tool_results`.
5. Traced execution: `harnx-engine::eval_tool_calls` builds `ToolResult::new()` (markdown None); live emit renders template for event but never writes back to the result object.
6. Mock tests hid the bug — they constructed `ToolResult { markdown: Some("test"), .. }` directly, bypassing the real production flow.

## Root Cause

The persistence seam has two independent paths:

1. **Live emit path**: Template markdown rendered directly into `ToolEvent::Completed` for SSE broadcast — does not touch `ToolResult`.
2. **Persistence path**: `ToolResult` constructed via `ToolResult::new()` with `markdown: None`, then saved via `append_session_tool_results`.

The field was added to the struct and the emit path, but never threaded through the persistence construction sites. Old serialized sessions deserialize with `None` (serde default), but new sessions also had `None` because nothing populated it before save.

**Affected construction/conversion sites:**
- `harnx_runtime::tool::execute_tool_round_with_persistence` (success + error paths)
- `harnx_runtime::nats_client_session::render_tool_results_entry` (thin-client replay)
- `harnx_core::session::ToolOutput` mirror (session.rs)
- `config/session.rs`, `nats_worker/agent_loop.rs`, `session_history.rs`, `session_reconstruct.rs`, `client/message.rs`, `compaction.rs`

## Solution

### 1. Populate before persist (the finalize-before-save seam)

Added `populate_result_markdown(results, eval_ctx)` call immediately before `append_session_tool_results`:

```rust
// crates/harnx-runtime/src/tool.rs:142-152
let results = populate_result_markdown(results, &eval_ctx);
if !dry_run {
    config.write().append_session_tool_results(&results)?;
}

// Error path also needs it:
let fallback = populate_result_markdown(fallback, &eval_ctx);
let _ = config.write().append_session_tool_results(&fallback);
```

`populate_result_markdown` iterates results, looks up `ToolDeclaration.result_template` via `eval_ctx.render.decl_map`, renders via `render_tool_result_template`, and sets `.markdown` when present.

### 2. Serde defaults for backward compatibility

```rust
// crates/harnx-core/src/tool.rs
pub struct ToolResult {
    pub call: ToolCall,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<MessageContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_agent: Option<SwitchAgentData>,
}
```

Old serialized sessions (without `markdown` field) deserialize to `None`. Older clients ignore unknown fields.

### 3. AG-UI custom event for live path

External `ag-ui-core` crate cannot be patched. Use additive custom event:

```rust
// crates/harnx-serve/src/ag_ui.rs
fn emit_tool_summary(&self, tool_call_id: String, markdown: String) {
    self.emit_custom(
        "tool_summary",
        json!({
            "tool_call_id": tool_call_id,
            "markdown": markdown,
        }),
    );
}

// Emit adjacent to ToolCallStart when Started.markdown is Some
if let Some(markdown) = markdown {
    self.emit_tool_summary(id.clone(), markdown);
}
```

Web client stores by `tool_call_id` in context; `ToolCallCard` resolves from either source.

### 4. Live vs restore dual carrier

```
LIVE:    tool_summary custom event → stored in React context → keyed by tool_call_id
RESTORE: history_messages_for_snapshot → prefers tool_result.markdown → baked into tool message content
```

Web `ToolCallCard` resolves from either:

```tsx
// web/src/ToolCallCard.tsx
const summaryMarkdown = toolSummaries.get(effectiveId) ||  // live path (custom event)
  (typeof result === 'object' && result?.markdown) ||       // restore path (baked into result)
  (typeof result === 'string' && result);                   // fallback
```

### 5. Extend usage event additively

```rust
let usage_context = self.session_usage_context();
let mut payload = json!({
    "input": input,
    "output": output,
    "cached": cached,
    "session_label": session_label,
});
if let Some(context) = usage_context {
    payload["context_tokens"] = json!(context.context_tokens);
    payload["max_context_tokens"] = json!(context.max_context_tokens);
    if let Some(percent) = context.context_percent {
        payload["context_percent"] = json!(percent);
    }
}
```

Never remove/rename fields; older clients ignore extras.

## Why This Works

1. **Populate before persist** ensures the field lands in the session log for all code paths (success, error, interrupt).
2. **Serde defaults** guarantee backward compatibility — old sessions deserialize, old clients ignore new fields.
3. **Custom event** avoids patching external crate and works with stock `@assistant-ui/react-ag-ui` parser.
4. **Dual carrier resolution** handles live (event-based) and restore (snapshot-based) uniformly.

## Prevention Strategies

**Test Cases:**
- End-to-end test: run execution → persist → restore → assert tool summary renders. Mock-based tests that hand-set the field do NOT catch this.
- Stress test: add new additive field + serde(default) → verify old serialized JSON still deserializes.
- Snapshot test: golden session log with `markdown: Some("...")` → compare after round-trip.

**Best Practices:**
- For ANY additive persisted field, test the FULL production data flow: execution → persistence → restore.
- Mock-constructed data hides seam bugs — always test the persist/restore barrier.
- Add `#[serde(default, skip_serializing_if = ...)]` on day one for any new `Option<_>` field on persisted structs.
- Custom AG-UI events (`tool_summary`, extended `usage`) are additive; never remove/rename existing fields.

**Code Review Checklist:**
- [ ] Additive field populated at ALL persistence sites (config/session.rs, nats_worker, session_history, compaction)?
- [ ] Serde default + skip_serializing_if for backward compatibility?
- [ ] End-to-end test covers execution → persist → restore?
- [ ] Live and restore paths both verified?

## assistant-ui v0.14.26 Specifics

- Tool render props expose `argsText` (raw JSON string), not just parsed `args`.
- `result`/`isError`/`status` union available; native `ToolGroup` is deprecated grouping only (no collapse API) → build custom disclosure.
- Use `key={agent:session}` on `AssistantRuntimeProvider` so switching re-inits runtime; constant key breaks switching.

## Overflow Guardrails

Wrap markdown tables in `overflow-x-auto`; use `whitespace-pre-wrap` + `break-words` on raw/view-source panes. Truncate `tool_call_id` to avoid formatting overflow (kagent #1360 → #1729 was reverted for this).

## Env/Tooling Gotchas

- Repo mandates `cargo nextest` (not `cargo test`).
- Pre-existing TUI panic-guard test hangs under nextest in sandbox; exclude via `-E 'not test(guard_drop_during_panic_does_not_double_panic)'`.
- Session dump test reads real `~/.config/harnx` — environmental flake; exclude similarly.
- Web: corepack pins pnpm 11.10.0. If env differs: `COREPACK_ENABLE_STRICT=0 pnpm --pm-on-fail=ignore`.

## Related Issues

- **GitHub:** [#1028](https://github.com/dobesv/harnx/issues/1028) — Web UI TUI-parity
- **Related Solution:** [logic-errors/session-save-format-consistency-2026-05-05.md](../logic-errors/session-save-format-consistency-2026-05-05.md) — persistence path divergence
- **Related Solution:** [integration-issues/ag-ui-tool-approval-interrupt-resume-2026-07-08.md](./ag-ui-tool-approval-interrupt-resume-2026-07-08.md) — AG-UI interrupt/resume patterns
- **kagent:** #1360 → #1729 (reverted) — overflow/formatting regression
