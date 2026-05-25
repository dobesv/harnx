---
title: "Added strict jq evaluation for package patches to prevent silent failures"
date: 2026-05-24
category: "logic-errors"
problem_type: logic_error
component: "patch-layer"
root_cause: "permissive jq evaluation silently ignored invalid patch expressions"
resolution_type: code_fix
severity: medium
tags:
  - jaq
  - jq
  - patching
  - error-propagation
  - package-config
plan_ref: "issue-623-invalid-jq-patch-error"
---

## Problem

Invalid jq expressions in harnx package patches (`.patch.yaml` files) were silently ignored. The `eval_filters()` function logged a warning and returned the input unchanged, so a misconfigured patch would silently have no effect. Agents, clients, and MCP servers would appear to load normally with wrong (unpatched) config.

## Symptoms

- Package patches with syntax errors (e.g., unclosed strings) produced no user-visible error
- Patched entities loaded with original configuration instead of patched values
- `jaq parse/compile failed` and `jaq runtime failed` appeared in logs but operation continued
- Debugging required digging through warn-level logs to find the root cause

```
# Example: Invalid expression silently ignored
patch:
  agents:
    my-agent:
      - '.model = "unclosed'  # Missing closing quote
# Result: Agent loads with original model, no error surfaced
```

## Investigation Steps

1. Reviewed `eval_filters()` in `crates/harnx-core/src/jaq.rs` — found it used `fold` with `unwrap_or(current)` pattern that silently skipped failures
2. Traced call sites: `apply_agent_patch`, `apply_mcp_server_patch`, `apply_client_patch` all used permissive variant
3. Noted that `harnx-client/src/client.rs` uses jq for request patching with different semantics — permissive behavior may be appropriate there
4. Decided to add strict variant rather than change existing behavior for request patching

## Root Cause

The `eval_filters()` function was designed to be resilient — on any jq parse/compile/runtime error, it logged a warning and returned the current accumulated value unchanged:

```rust
pub fn eval_filters(exprs: &[String], input: Value) -> Value {
    exprs.iter().fold(input, |current, expr| {
        eval_filter(expr, current.clone()).unwrap_or(current)
    })
}
```

This behavior was appropriate for request patching where partial success is acceptable, but dangerous for config patches where silent failure means wrong configuration loads.

## Solution

Added `eval_filters_strict()` variant that propagates errors:

```rust
/// Like eval_filters, but returns Err on any expression parse/compile/runtime error.
pub fn eval_filters_strict(exprs: &[String], input: Value) -> anyhow::Result<Value> {
    exprs
        .iter()
        .try_fold(input, |current, expr| eval_filter_strict(expr, current))
}

fn eval_filter_strict(expr: &str, input: Value) -> anyhow::Result<Value> {
    let filter = compile_filter(expr)
        .map_err(|e| anyhow::anyhow!("jq compile error in {:?}: {}", expr, e))?;
    run_filter(&filter, input)
        .ok_or_else(|| anyhow::anyhow!("jq runtime error in {:?}: no output", expr))
}
```

Updated all package patch functions to use strict variant:

- `apply_agent_patch` in `crates/harnx-runtime/src/config/agent.rs` — now returns `Result<()>`
- `apply_mcp_server_patch` in `crates/harnx-runtime/src/config/mod.rs` — now returns `Result<()>`
- `apply_client_patch` in `crates/harnx-runtime/src/config/mod.rs` — now returns `Result<()>`

Package loaders skip entities whose patches fail and log entity-contextual errors:

```rust
// In load_package_servers
if let Err(e) = apply_mcp_server_patch(&mut server, &patch.servers) {
    log::error!("Package patch failed for MCP server '{}': {e:#}", server.name);
    continue; // Skip this server
}
```

### Key Design Decision

Kept permissive `eval_filters()` unchanged for request patching in `harnx-client`. Request data patching has different failure semantics — partial success is often acceptable. Only package config patches require strict validation.

## Why This Works

- `try_fold` short-circuits on first error, stopping at the offending expression
- `anyhow::Result` propagates up through `apply_*_patch` functions with context
- Loader level logs entity name before skip, providing actionable debugging info
- Original config preserved on failure — no partial mutation

## Prevention Strategies

**Test Cases Added:**
- `eval_filters_strict_returns_err_on_invalid_expression` — validates core strict function
- `eval_filters_strict_applies_valid_expressions` — validates successful path
- `apply_agent_patch_with_invalid_jq_expression_returns_err` — agent patch path
- `apply_mcp_server_patch_with_invalid_jq_expression_returns_err` — MCP server patch path
- `apply_client_patch_with_invalid_jq_expression_returns_err` — client patch path

**Best Practices:**
- Use strict variant for config patches where invalid input means misconfiguration
- Use permissive variant for request patching where resilience is desired
- Always log entity name when skipping due to patch failure

**Code Review Checklist:**
- [ ] New jq evaluation uses appropriate variant (strict vs permissive)
- [ ] Error messages include expression text for debugging
- [ ] Loader level preserves original config on patch failure

## Related Issues

- **GitHub:** #623 — Invalid jq patch error
- **Related Solution:** [jaq-expression-patching-yaml-to-jq-migration-2026-05-16.md](../integration-issues/jaq-expression-patching-yaml-to-jq-migration-2026-05-16.md) — Original jaq patch system introduction
