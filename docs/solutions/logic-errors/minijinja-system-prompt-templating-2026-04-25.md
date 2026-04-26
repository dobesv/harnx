---
title: "MiniJinja templating for agent system prompts"
date: 2026-04-25
category: "logic-errors"
problem_type: logic_error
component: "harnx-core agent system"
root_cause: "regex-based interpolation lacked rich context, error handling, and extensibility"
resolution_type: code_fix
severity: medium
tags:
  - minijinja
  - templating
  - system-prompt
  - result-propagation
  - rust
plan_ref: "harnx-339-minijinja-system-prompt"
---

## Problem

Agent system prompts used a regex-based `interpolate_variables()` function that only supported a fixed set of system variables (`__os__`, `__arch__`, etc.). It couldn't render agent metadata (name, model) or user-defined variables, and unknown placeholders passed through silently rather than surfacing configuration errors.

## Symptoms

```
- Templates with unknown {{variable}} rendered with placeholder intact, no error
- No way to reference agent.name, agent.model in system prompts
- User-defined variables from agent configs weren't interpolated
- Template errors discovered late at runtime, not at session creation
```

## Investigation Steps

1. Identified `interpolate_variables()` in `crates/harnx-core/src/system_vars.rs` as the regex-based interpolator
2. Noted `fancy_regex` dependency usage limited to this function
3. Considered MiniJinja for richer templating (conditionals, loops, nested object access)
4. Discovered `context!` macro limitation: requires static keys at compile time, incompatible with dynamic user-defined variables
5. Traced `interpolated_instructions()` call graph: `system_text()` → `build_messages()` → `echo_messages()` → `set_agent()` → `sync_agent()` → `session::new()`
6. Identified timing bug: `session::new()` calls `set_agent()` which renders template before file-backed agent variables initialized

## Root Cause

**Regex limitation**: `interpolate_variables()` used `fancy_regex::Regex::replace_all()` with a callback that returned `{{{key}}}` for unknown keys. This silent pass-through masked configuration errors.

**Static context macro**: MiniJinja's `context!` macro is a compile-time construct requiring literal key names. Cannot mix static system vars (`__os__`) with dynamic user vars from `AgentVariables` map.

**Initialization timing**: In `use_agent_by_name()`, the call order was:
1. `resolve_file_defaults()` - loads agent definition
2. `session::new()` → `set_agent()` → `render_template()`
3. `init_agent_session_variables()` - too late, template already rendered

## Solution

Replaced regex interpolation with MiniJinja template rendering using `BTreeMap<String, Value>` for dynamic context:

**Before (regex):**
```rust
pub fn interpolate_variables(input: &str) -> String {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"\{\{([a-zA-Z_][a-zA-Z0-9_]*)\}\}").unwrap()
    });
    RE.replace_all(input, |caps: &Captures| {
        let key = &caps[1];
        match key {
            "__os__" => env::consts::OS.to_string(),
            // ... other static vars ...
            _ => format!("{{{key}}}"), // Silent pass-through
        }
    }).to_string()
}
```

**After (MiniJinja):**
```rust
pub fn render_template(template: &str, agent: &AgentConfig) -> Result<String> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);

    let agent_ctx = AgentContext {
        name: agent.name(),
        model: agent.model_id(),
        // ... other agent fields ...
    };

    let mut ctx: BTreeMap<String, Value> = BTreeMap::new();
    ctx.insert("__os__".to_string(), Value::from(env::consts::OS));
    ctx.insert("__os_family__".to_string(), Value::from(env::consts::FAMILY));
    // ... other system vars ...
    ctx.insert("agent".to_string(), Value::from_serialize(&agent_ctx));

    for (k, v) in agent.variables() {
        ctx.insert(k.clone(), Value::from(v.clone()));
    }

    env.render_str(template, ctx)
        .map_err(|e| anyhow::anyhow!("Template error in agent '{}': {}", agent.name(), e))
}
```

**Result propagation cascade:**
```rust
// Changed signatures across harnx-core and harnx-runtime:
pub fn interpolated_instructions(&self) -> Result<String>
pub fn system_text(&self) -> Result<String>
pub fn build_messages(&self, input: &Input) -> Result<Vec<Message>>
pub fn set_agent(&mut self, agent: &AgentConfig) -> Result<()>
pub fn sync_agent(&mut self, agent: &AgentConfig) -> Result<()>
pub fn new(config: &Config, name: &str) -> Result<Session>
```

**Timing fix (in `use_agent_by_name()`):**
Pre-populate `shared_variables` after `resolve_file_defaults()` before calling `session::new()`:
```rust
// After resolve_file_defaults(), before session::new():
let file_vars = init_agent_variables(&agent);
agent.set_shared_variables(file_vars);
// Now session::new() can render template with variables already available
```

## Why This Works

**BTreeMap for dynamic context**: `BTreeMap<String, Value>` accepts any string key at runtime, allowing mix of system vars (`__os__`), agent object (`agent`), and user-defined vars from `AgentVariables`.

**UndefinedBehavior::Strict**: Forces explicit failure on `{{undefined_var}}`, surfacing configuration errors at session creation rather than serving broken prompts to users.

**Result propagation**: Returns `Result` from `render_template()` through entire call chain, ensuring template errors halt session creation with clear error message.

**Pre-initialization**: Moving variable population before `session::new()` ensures template context is complete when `set_agent()` triggers rendering.

## Prevention Strategies

**Test Cases:**
```rust
#[test]
fn test_render_template_undefined_var_returns_err() {
    let agent = AgentConfig::from_prompt("Hello {{undefined_var}}");
    let result = agent.interpolated_instructions();
    assert!(result.is_err());
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("Template error"));
}
```

**Code Review Checklist:**
- [ ] Template variables documented in agent configs match `AgentVariables` schema
- [ ] New template context fields added to `AgentContext` struct and `render_template()` map
- [ ] Test coverage for both defined and undefined variable cases
- [ ] File-backed agent variables loaded before session creation

**Monitoring:**
- Log `Template error` occurrences during session creation
- Track `session::new()` failures attributed to template rendering

## Related Issues

- **Plan:** harnx-339-minijinja-system-prompt
- **Commit:** bfc51ca — Implement MiniJinja templating for system prompts
- **Breaking change:** `UndefinedBehavior::Strict` means templates with undefined placeholders will error instead of rendering partially
