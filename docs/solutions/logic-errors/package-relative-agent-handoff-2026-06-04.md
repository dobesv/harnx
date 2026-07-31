---
title: "Package-relative agent handoff resolution for _session_handoff"
date: 2026-06-04
category: logic-errors
problem_type: logic_error
component: harnx-engine
root_cause: missing-package-context-in-synthetic-tool-dispatch
resolution_type: code_fix
severity: medium
tags:
  - session-handoff
  - packages
  - namespacing
  - agent-resolution
plan_ref: issue-709-package-handoff
---

## Problem

A package agent (`pantheon/daedalus`) using a bare `_session_handoff` tool (`atlas_session_handoff`) to hand off to a same-package agent resolved the target to top-level `atlas` instead of `pantheon/atlas`. The synthetic handoff dispatch had no awareness of the active agent's package.

## Symptoms

- Package agent calls `atlas_session_handoff`, expecting `pantheon/atlas`
- Handoff lands on top-level `atlas` if it exists, or fails if only `pantheon/atlas` exists
- Confusing behavior: same tool call produces different results from package vs top-level agents
- Issue #709: explicit bug report for same-package handoff misresolution

## Investigation Steps

1. Traced handoff dispatch in `crates/harnx-engine/src/tool.rs`: the `_session_handoff` branch extracted the agent name by trimming the `_session_handoff` suffix and emitted `switch_agent { agent }` verbatim.

2. Verified downstream `Config::use_agent` → `agent::init` → `Config::agent_file` resolution rules:
   - Name containing `/` → `packages/<pkg>/agents/<stem>.md`
   - Bare name → `agents/<name>.md`

3. Confirmed bare names always resolved top-level regardless of calling agent's package.

4. Found existing helper `harnx_core::package_namespace::resolve_package_relative_name()` implementing exactly the needed resolution semantics:
   - `/foo` → `foo` (top-level escape)
   - `other/foo` → `other/foo` (cross-package)
   - `foo` + `Some(pkg)` → `pkg/foo` (same-package)
   - `foo` + `None` → `foo` (top-level context)


## Root Cause

The `_session_handoff` dispatch in `dispatch_tool_call` extracted the agent name from the tool name and passed it directly to the result without package-relative transformation. The dispatcher had no access to the active agent's package context.

**Data flow gap:**

```
Input.agent().name() → "pantheon/daedalus"  (package info here)
    ↓
execute_tool_round → ToolEvalContext built without package field
    ↓
dispatch_tool_call → _session_handoff branch sees only bare "atlas"
    ↓
switch_agent result → "atlas" (wrong, should be "pantheon/atlas")
```

## Solution

### 1. Added Package Context to ToolEvalContext

In `crates/harnx-engine/src/tool.rs`:

```rust
pub struct ToolEvalContext {
    // ... existing fields ...
    /// Package of the currently active agent (e.g. `Some("pantheon")` for
    /// `pantheon/daedalus`, `None` for a top-level agent). Used to resolve
    /// bare `_session_handoff` targets relative to the current package.
    pub current_agent_package: Option<String>,
}
```

### 2. Derived Package Context at Runtime Layer

In `crates/harnx-runtime/src/tool.rs`:

```rust
// In execute_tool_round:
let current_agent_package =
    harnx_core::package_namespace::pkg_from_qualified(input.agent().name())
        .map(str::to_string);

let eval_ctx = build_tool_eval_context(
    config,
    agent_use_tools.as_deref(),
    current_agent_package,  // new parameter
    persistent_manager,
);
```

### 3. Applied Resolution in Handoff Dispatch

In `crates/harnx-engine/src/tool.rs`:

```rust
if call.name.ends_with("_session_handoff") {
    // Strip exactly one suffix (not all repeats)
    let bare_target = call.name
        .strip_suffix("_session_handoff")
        .unwrap_or(&call.name);

    // Resolve relative to current package
    let agent = harnx_core::package_namespace::resolve_package_relative_name(
        bare_target,
        ctx.current_agent_package.as_deref(),
    );

    // ... rest of handoff logic uses resolved `agent`
}
```

### 4. Layering Preserved

- `harnx-runtime`: derives package context from `Input`, passes to context builder
- `harnx-engine`: consumes context, applies resolution via shared helper
- `harnx-core`: pure resolution logic, no I/O dependency

Dependency direction runtime → engine → core maintained correctly.

## Why This Works

The `resolve_package_relative_name()` helper encapsulates the four resolution rules already used for compaction agent and client/model resolution (documented in [package-scoped-name-resolution-2026-05-17.md](./package-scoped-name-resolution-2026-05-17.md)). By threading the active agent's package through `ToolEvalContext`, the handoff dispatcher can apply the same rules:

| Tool Call | From Agent | Package Context | Resolved Target |
|-----------|------------|-----------------|-----------------|
| `atlas_session_handoff` | `pantheon/daedalus` | `Some("pantheon")` | `pantheon/atlas` |
| `atlas_session_handoff` | `atlas` (top-level) | `None` | `atlas` |
| `/atlas_session_handoff` | `pantheon/daedalus` | `Some("pantheon")` | `atlas` (escape) |
| `other/atlas_session_handoff` | `pantheon/daedalus` | `Some("pantheon")` | `other/atlas` |

The `strip_suffix()` call removes exactly one `_session_handoff` suffix, preventing misresolution of edge-case agent names ending with `_session_handoff` (unlike `trim_end_matches()` which greedily removes repeated suffixes).

## Prevention Strategies

### Test Cases

Added 6 unit tests in `crates/harnx-engine/src/tool.rs`:

- `handoff_bare_target_resolves_to_current_package` — main fix verification
- `handoff_bare_target_top_level_unchaved` — `None` package context (regression guard)
- `handoff_leading_slash_escapes_to_top_level` — `/` escape hatch
- `handoff_qualified_target_unchanged` — cross-package pass-through
- `handoff_strips_only_one_suffix` — edge case: agent named `*_session_handoff`
- `handoff_propagates_prompt` — contract verification: prompt flows through

### Code Review Checklist

- [ ] Does synthetic tool dispatch account for package context where relevant?
- [ ] When extending `ToolEvalContext`, update all `build_tool_eval_context` call sites
- [ ] Use `strip_suffix()` for single-suffix removal, not `trim_end_matches()`
- [ ] Package context derives from `Input.agent().name()` per-prompt, not global config

### Key Gotchas

1. **`strip_suffix` vs `trim_end_matches`**: `trim_end_matches` removes *all* trailing matches of the pattern. For `_session_handoff`, this could misresolve an agent literally named `atlas_session_handoff` (tool `atlas_session_handoff_session_handoff` would resolve to `atlas` instead of `atlas_session_handoff`). Use `strip_suffix()` for exact single-suffix removal.


3. **Declarations were also wrong (invalid tool names)**: The initial fix corrected *resolution* but left the declaration layer emitting `format!("{agent_name}_session_handoff")` from `list_agents()` — which returns `pkg/stem`. That produced literal `pantheon/atlas_session_handoff` tool names containing a `/`, which is **invalid** for OpenAI/Anthropic function-name schemas (`^[a-zA-Z0-9_-]+$`) and harnx does no client-side sanitization. See the follow-up below.

## Follow-up: package-aware handoff tool-name spelling

### Why string-munging the name is unsafe
`validate_package_name` allows `[a-zA-Z0-9_-]`, so **package names may contain `_`**, and agent stems may too. Decoding a display name like `a__b__c` back to `pkg/stem` by splitting on `__` is therefore ambiguous. The decode **must** use an exact lookup map, never string parsing.

### Spelling scheme (LLM-visible display name, relative to the active agent's package)
All names are slash-free and match `[A-Za-z0-9_-]`:

| Target (from active package `P`) | Display name | Decodes to |
|---|---|---|
| Same-package peer `P/atlas` | `atlas` | `P/atlas` |
| Cross-package peer `other/helper` | `other__helper` | `other/helper` |
| Top-level agent `global` | `__global` | `global` |
| (active agent is top-level) package peer `P/atlas` | `P__atlas` | `P/atlas` |
| (active agent is top-level) top-level peer `global` | `global` | `global` |

Helper: `harnx_core::package_namespace::handoff_display_name(target, active_pkg)`.

### Decode (engine): map value is canonical — use it VERBATIM
`ToolEvalContext.handoff_targets: HashMap<String,String>` maps display-name → qualified-or-bare agent name. The engine does an exact lookup and uses the value **as-is**. Do NOT re-run `resolve_package_relative_name` on a map hit: a top-level value like bare `global` would be wrongly qualified to `P/global`. Re-resolution applies only on the fallback path (empty map / legacy / test contexts).

### The trap: declaration generation and tool selection must use the SAME package context
The handoff declaration names are generated in three places that all feed the LLM or the engine allow-list:
- `build_tool_eval_context` → the engine's `allowed_tool_names` + `handoff_targets`.
- `select_tools(agent)` (via `config/input.rs`) → the function list **sent to the LLM**.
- `active_tool_names()` → UI/whitelist display.

If any of these passes `None` for the package while another passes the real package, the LLM sees one spelling (e.g. `P__atlas_session_handoff`) while the engine allow-lists another (bare `atlas_session_handoff`) → the call fails the `allowed_tool_names` gate. **All declaration-generation call sites must derive the active agent's package** via `pkg_from_qualified(agent.name())`.

## Related Issues

- **GitHub Issue:** [#709](https://github.com/dobesv/harnx/issues/709) — Agent handoff from package agent goes to non-package agent
- **Prior Solution:** [package-scoped-name-resolution-2026-05-17.md](./package-scoped-name-resolution-2026-05-17.md) — Same resolution helper applied to compaction agents and client/model references
