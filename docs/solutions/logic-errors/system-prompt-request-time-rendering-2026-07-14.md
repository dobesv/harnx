---
title: "System prompt rendered at request time with model/tool/env awareness"
date: 2026-07-14
category: "logic-errors"
problem_type: logic_error
component: "harnx-core agent system, harnx-runtime session handling"
root_cause: "stored rendered prompt in transcript prevented dynamic re-rendering after model fallback or tool changes"
resolution_type: code_fix
severity: medium
tags:
  - system-prompt
  - minijinja
  - compaction
  - session-transcript
  - model-fallback
  - tools
plan_ref: "harnx-system-prompt-refactor"
---
## Problem

Agent system prompts were persisted in session transcripts as rendered text. This prevented prompts from adapting to model fallback, tool changes, or environment variable updates during resumed sessions. Compaction baked the full system prompt into summary markers, polluting TUI display.

## Symptoms

```
- `session.yaml` contains `role: system` entries with frozen prompt text
- After model fallback, `{{ agent.model }}` in prompt still shows original model
- Compaction markers in TUI show full system prompt text, obscuring the summary
- Resumed sessions cannot render `{{ tools }}` because tool list resolved at session creation
- Issues: #1024 (compaction marker noise), #487 (model-aware prompt customization)
```

## Investigation Steps

1. Traced `append_initial_agent_messages` → `build_messages` → stored System message in transcript
2. Found `compress` and `compress_keeping_recent` prepend system prompt to summary and push synthetic System message to `session.messages`
3. Identified TUI assumed `session.messages[0].role == System` for compaction marker
4. Verified `build_messages` rendered prompt once at session creation, not per-request
5. Discovered `prepare_completion_data` called `select_tools` AFTER `build_messages`, so `{{ tools }}` was always empty

## Root Cause

The system prompt was rendered once during session initialization and persisted to the `SessionLogEntry::Message { role: System }` transcript entry. Subsequent LLM requests used the stored text, not re-rendering from the template. This architecture:

- Locked `{{ agent.model }}` to the initial model name
- Prevented `{{ tools }}` from reflecting runtime tool selection
- Required compaction to preserve the system prompt as a synthetic `Message { role: System }` at `session.messages[0]`
- Created a positional TUI assumption that `active_messages[0]` was the compaction summary

## Solution

### Core architectural change: render at request time, never store

**1. Stop persisting System messages in transcript**

```rust
// append_initial_agent_messages now filters System:
for mut msg in agent_messages {
    if msg.role == MessageRole::System {
        continue; // don't log to session
    }
    // ... append user/assistant messages
}
```

**2. Replay skips legacy stored System when raw template present**

```rust
// replay_log_entries_for_external
SessionLogEntry::Message { role, .. } if role == MessageRole::System => {
    if !session.agent_instructions.is_empty() {
        // have raw template → skip stored render, inject fresh later
        continue;
    }
    // legacy fallback: load as-is
}
```

**3. `inject_system_prompt` defaults true**

```rust
// Input::new
inject_system_prompt: true,  // was false
```

Every request path goes through `build_messages_inner`, which injects a fresh render at index 0.

**4. Compaction: runtime-only `compaction_summary` replaces synthetic System message**

```rust
// Session struct
#[serde(skip)]
pub compaction_summary: Option<String>,

// compress/compress_keeping_recent
session.compaction_summary = Some(prompt.clone());  // no System message push

// TUI build_transcript_with_compaction
fn build_transcript_with_compaction(
    compaction_summary: Option<&str>,  // explicit param, not positional lookup
    ...
) -> Vec<TranscriptItem>
```

**5. Tools + model in Jinja context**

```rust
// render_template signature
pub fn render_template(
    template: &str,
    agent: &AgentConfig,
    tools: Option<&[ToolDeclaration]>,  // ToolDeclaration already derives Serialize
) -> Result<String>

// Input gains field
#[serde(skip)]
pub resolved_tools: Option<Vec<ToolDeclaration>>,

// prepare_completion_data: select_tools BEFORE build_messages
let functions = config.read().select_tools(input.agent());
input.resolved_tools = functions.clone();
let messages = build_messages(input, config)?;

// build_messages_inner
let system_text = input.agent().system_text_with_tools(
    input.resolved_tools.as_deref().unwrap_or_default()
)?;
```

**6. Model fallback: update before prompt render**

```rust
// retry.rs: call_with_retry_and_fallback_custom
let selected_model = client.model().clone();
// ordering: model updated before prompt render so {{ agent.model }} reflects fallback
(ctx.select_model_fn)(input, &selected_model);
// NOW call prepare_completion_data → build_messages → render
```

## Pitfalls Discovered

### 1. Store RAW template in `agent_instructions`, not rendered prompt

**Bug:** `Session::set_agent` and `sync_agent` stored `agent.interpolated_instructions()` (rendered text) in `agent_instructions`, defeating future re-render.

**Fix:** Store `agent.instructions_template()` (raw template string).

```rust
fn set_agent(&mut self, agent: &AgentConfig) {
    self.agent_instructions = agent.instructions_template().to_string();  // raw, not rendered
}
```

**Why it matters:** If `agent_instructions` holds rendered text, `{{ agent.model }}` is frozen to the model name at session creation, not re-evaluated after model change.

### 2. Legacy sessions: strip stored System before fresh injection

**Bug:** Legacy sessions (empty `agent_instructions`, named agent retrievable) have a stored `role: system` entry. With `inject_system_prompt` defaulting to true, `build_messages_inner` would prepend a fresh System message, yielding `[fresh System, stored System, ...User]` to the LLM.

**Fix:** Strip leading `MessageRole::System` entries before injecting fresh prompt:

```rust
if input.inject_system_prompt() {
    let system_text = input.agent().system_text_with_tools(...)?;
    if !system_text.is_empty() {
        // strip legacy stored System messages
        while matches!(messages.first().map(|m| m.role), Some(MessageRole::System)) {
            messages.remove(0);
        }
        messages.insert(0, Message::new(MessageRole::System, ...));
    }
}
```

### 3. `to_agent_config` must use `from_prompt`, not `from_markdown`

**Bug:** Raw templates starting with `---` (common in frontmatter-like instructions) were misparsed by `AgentConfig::from_markdown`, which interpreted `---` as YAML frontmatter delimiter.

**Fix:** Use `from_prompt` for session-resume path, which treats the input as verbatim prompt text:

```rust
pub fn to_agent_config(&self) -> Result<AgentConfig> {
    let prompt = if self.agent_instructions.is_empty() {
        self.agent_prompt.as_str()
    } else {
        self.agent_instructions.as_str()  // prefer raw template
    };
    let mut config = AgentConfig::from_prompt(prompt);  // not from_markdown
    config.set_name(agent_name);
    // ... restore other fields from session metadata
}
```

### 4. `system_text` must honor `instructions` override

**Bug:** Early implementation rendered `self.prompt` directly, ignoring `self.instructions` frontmatter override. Agents with `instructions:` field in agent markdown had that content ignored.

**Fix:** Use `instructions_template()` helper that returns `self.instructions.as_deref().unwrap_or(&self.prompt)`.

### 5. Workspace test runner OOM in constrained environments

**Observation:** `cargo nextest run --workspace` compiles all test binaries in parallel. Large crates (harnx-runtime e2e tests) cause linker OOM in memory-constrained sandboxes.

**Workaround:** Run tests per-crate: `cargo nextest run -p harnx-core`, `cargo nextest run -p harnx-runtime`, etc.

**Pre-existing test hang:** `harnx-tui::guard_drop_during_panic_does_not_double_panic` hangs in sandbox environments lacking a real TTY. Exclude with `-E 'not test(guard_drop_during_panic_does_not_double_panic)'`.

## Why This Works

**Request-time rendering:** Each LLM call renders the prompt from the raw template with current context. Model changes, tool updates, and environment variables always reflect current state.

**Single source of truth:** `agent_instructions` stores raw template; `AgentConfig` receives it via `to_agent_config`; `build_messages_inner` renders with current `agent.model` and `resolved_tools`.

**Explicit threading:** `compaction_summary` passed as explicit parameter, eliminating positional `session.messages[0]` assumption. Backward compat achieved via conditional replay logic.

**No mirror struct needed:** `ToolDeclaration` already derives `Serialize`. Skipped fields (`mcp_tool_name`, `call_template`) don't appear in Jinja context, matching intended design.

## Prevention Strategies

**Test Cases:**
```rust
// New session: no role:system stored
#[test]
fn first_turn_persists_user_only_and_no_stored_system_message()

// Compaction: summary only, compaction_summary set
#[test]
fn compress_keeping_recent_log_stores_summary_only_and_runtime_tracks_it()

// Legacy session: stored System replaced, not duplicated
#[test]
fn build_messages_replaces_legacy_stored_system_prompt_with_fresh_render()

// Raw template stored for re-render fidelity
#[test]
fn to_agent_prefers_raw_agent_instructions_over_stale_agent_prompt_on_resume()

// Model-change triggers re-render
#[test]
fn interpolated_instructions_renders_current_model_id_after_model_change()

// Tools in Jinja
#[test]
fn system_text_with_tools_renders_tool_names_in_jinja()

// Template with --- preserved
#[test]
fn to_agent_config_preserves_prompt_starting_with_frontmatter_delimiter()
```

**Code Review Checklist:**
- [ ] `agent_instructions` stores raw template, not rendered output
- [ ] `to_agent_config` uses `from_prompt`, not `from_markdown`
- [ ] Legacy System message stripping before fresh injection
- [ ] `system_text`/`system_text_with_tools` use `instructions_template()` helper
- [ ] `prepare_completion_data` calls `select_tools` before `build_messages`
- [ ] Model update callback occurs before prompt render in retry loop
- [ ] `compaction_summary` threaded explicitly, not via positional assumption

**Monitoring:**
- Log template render errors during request preparation
- Track `{{ agent.model }}` mismatches between prompt and actual model on fallback
- Alert on empty system prompts (would indicate injection path failure)

## Related Issues

- **Issues:** [#1024](https://github.com/dobesv/harnx/issues/1024), [#487](https://github.com/dobesv/harnx/issues/487)
- **Prior solution:** [minijinja-system-prompt-templating-2026-04-25.md](./minijinja-system-prompt-templating-2026-04-25.md) — MiniJinja migration foundation
- **Changeset:** `.changeset/system-prompt-runtime-rendering.md`
