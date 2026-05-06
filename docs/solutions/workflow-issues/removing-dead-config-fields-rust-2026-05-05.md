---
title: "Removing dead configuration fields in Rust — complete checklist"
date: 2026-05-05
category: "workflow-issues"
problem_type: workflow_issue
component: "configuration-system"
root_cause: "feature-deprecation cumulative-touchpoint-tracking"
resolution_type: code_fix
severity: low
tags:
  - rust
  - serde
  - configuration
  - dead-code
  - refactoring
  - removal-checklist
plan_ref: "rm-default-session"
---

## Problem

Removing configuration fields from a Rust project is error-prone. Developers miss call sites, tests, documentation, or dead helper functions. The `*_default_session` fields (`tui_default_session`, `cmd_default_session`, `agent_default_session`, plus legacy `repl_default_session` alias) had accumulated touchpoints across multiple crates and documentation files.

## Symptoms

- Config fields referenced in code, docs, and tests after removal attempts
- Dead helper functions left behind (e.g., `expand_session_variables`, `sanitize_session_name`)
- User docs still mention removed config keys
- Tests fail when exercising removed features

## Investigation Steps

1. Located all struct fields with `*_default_session` name pattern
2. Traced serde aliases (`#[serde(alias = "repl_default_session")]`)
3. Searched for `Default` impl initializers
4. Grep'd for all accessor methods and call sites
5. Identified runtime env-var reading (`HARNX_TUI_DEFAULT_SESSION`, etc.)
6. Found helper functions used only by removed feature (session name expansion/sanitization)
7. Verified docs grep for removed key names
8. Checked example configs and agent frontmatter

## Root Cause

Configuration fields in Rust projects touch many layers: struct definition, Default impl, serde attributes, accessors, builder wiring, runtime env-var reading, call sites, tests, and documentation. No automated tool catches all of these. `pub` functions don't trigger `dead_code` warnings even with zero external callers.

## Solution

Applied systematic removal checklist:

**1. Struct definition (`config_data.rs`):**
```rust
// REMOVED
#[serde(alias = "repl_default_session")]
pub tui_default_session: Option<String>,
pub cmd_default_session: Option<String>,
pub agent_default_session: Option<String>,
```

**2. Default impl:**
```rust
// REMOVED from Default impl
tui_default_session: None,
cmd_default_session: None,
agent_default_session: None,
```

**3. Serde alias test:**
```rust
// REMOVED test
#[test]
fn repl_default_session_alias_still_works() { ... }
```

**4. AgentConfig struct and frontmatter (`agent_config.rs`):**
```rust
// REMOVED field
agent_default_session: Option<String>,

// REMOVED accessor
pub fn agent_default_session(&self) -> Option<&str> { ... }

// REMOVED from is_empty() check
&& self.agent_default_session.is_none()
```

**5. SystemVars template struct (`system_vars.rs`):**
```rust
// REMOVED from AgentContext struct
agent_default_session: Option<&'a str>,
```

**6. Runtime config wiring (`config/mod.rs`):**
- Removed from `apply_agent()` session resolution logic
- Removed `apply_default_session()` method entirely
- Removed env-var reading for `HARNX_TUI_DEFAULT_SESSION`, `HARNX_CMD_DEFAULT_SESSION`, `HARNX_AGENT_DEFAULT_SESSION`

**7. Entrypoint call (`main.rs`):**
```rust
// REMOVED
config.write().apply_default_session()?;
```

**8. Dead helper functions (`session_name.rs`):**
Removed entire module content:
- `expand_session_variables()`
- `expand_session_variables_with()`
- `sanitize_session_name()`
- All associated unit tests

**9. Documentation:**
- Configuration guide: removed "Default Session" section
- Agent guide: removed `agent_default_session` from frontmatter table
- Example config: removed `*_default_session` keys
- Example agent frontmatter: removed `agent_default_session`

**10. Changeset:**
Created `.changesets/474-remove-default-session.md` documenting migration path.

## Why This Works

Serde silently ignores unknown fields during YAML deserialization. Users with old config files containing `tui_default_session` won't get errors — keys are silently dropped. No migration tooling required for optional config fields.

Complete removal prevents confusion from partial deprecation. Users get clear feedback: feature doesn't exist, not "deprecated but still works sometimes."

## Prevention Strategies

**Removal Checklist for Rust Config Fields:**

- [ ] Struct definition (data model crate)
- [ ] `Default` impl initializer
- [ ] Serde aliases (`#[serde(alias = "...")]`)
- [ ] Accessor methods / getters
- [ ] Builder fields and wiring
- [ ] `is_empty()`-style check conditions
- [ ] Related template/system variable structs
- [ ] Runtime env-var reading (`HARNX_*` patterns)
- [ ] All call sites of the feature
- [ ] Entrypoint call (e.g., `main.rs`)
- [ ] Tests exercising the removed feature
- [ ] Example config files
- [ ] Example agent frontmatter
- [ ] User-facing docs (grep for key names)
- [ ] Dead helper functions (grep for callers after removal)
- [ ] Changeset file

**Grep Commands:**

```bash
# After removal, verify no lingering references:
rg -i "default_session" crates/ docs/ example_config/
rg -i "repl_default_session" crates/ docs/

# Check for dead pub functions:
rg "expand_session_variables" crates/
rg "sanitize_session_name" crates/
```

**Code Review Checklist:**

- [ ] Did you grep docs/ for removed config key names?
- [ ] Did you check for dead helper functions after removing call sites?
- [ ] Did you remove associated tests, not just the feature code?
- [ ] Did you create a changeset documenting migration path?

## Related Issues

- **Issue:** [dobesv/harnx#474](https://github.com/dobesv/harnx/issues/474) — Remove all *_default_session related features
- **Related Solution:** [logic-errors/mcp-plans-rewrite-patterns-2026-05-04.md](../logic-errors/mcp-plans-rewrite-patterns-2026-05-04.md) — Another example of dead code removal after feature changes
