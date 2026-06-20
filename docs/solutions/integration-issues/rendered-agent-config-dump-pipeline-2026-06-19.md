---
title: "Rendered agent config dump pipeline — CLI/TUI unified rendering"
date: 2026-06-19
category: "integration-issues"
problem_type: integration_issue
component: "harnx-runtime, harnx-core, harnx-tui, harnx-cli"
root_cause: "no unified path to render fully-resolved agent configuration for inspection/debugging"
resolution_type: code_fix
severity: medium
tags:
  - agent-config
  - rendering
  - cli
  - tui
  - mcp
  - tool-expansion
  - session
plan_ref: "issue-622-dump-rendered-agent-config"
---

## Problem

No unified mechanism existed to render a fully-resolved agent configuration (patches applied, templates interpolated, tool wildcards expanded) for inspection. Debugging agent behavior required manually tracing through multiple subsystems or inspecting raw YAML files that didn't reflect the actual runtime configuration.

## Symptoms

```
- Users couldn't see expanded tool list after wildcard resolution (e.g., fs:* → fs_read, fs_write, ...)
- Package-patched agents required manual jaq filter inspection to understand final config
- Template variables shown uninterpolated in source files
- No way to inspect session state without launching a full session
- CLI and TUI had different codepaths risking divergence
```

## Investigation Steps

1. Identified the agent rendering pipeline: `AgentConfig::from_markdown` → `apply_package_agent_transforms` (jaq filters) → `set_shared_variables` → `interpolated_instructions()`
2. Found `use_tools` wildcards require live MCP manager to expand via `tool_declarations_for_use_tools`
3. Discovered existing `AgentConfig::export()` returns raw front-matter without interpolation
4. Traced session storage paths: agent-scoped `<data_dir>/agents/<agent>/sessions/` vs top-level `<state_dir>/sessions/`
5. Found `Config::default()` has zero clients — model lookup fails in production session dumps

## Root Cause

The agent configuration pipeline was fragmented across multiple crates with no single orchestration function. Template interpolation happened deep in `interpolated_instructions()`, package patches applied separately in loader code, and tool wildcard expansion required live MCP state. No function assembled all these steps in the correct order for inspection purposes.

## Solution

Created unified rendering pipeline with two entry points:

### 1. Agent Dump: `render_agent_dump(config, agent_name)`

Located in `crates/harnx-runtime/src/config/agent.rs`:

```rust
pub fn render_agent_dump(config: &Config, agent_name: &str) -> Result<String> {
    // 1. Load agent config (file or builtin)
    let mut agent_config = load_agent_by_name(agent_name)?;
    
    // 2. Apply package patches BEFORE interpolation
    if let Some((pkg, stem)) = agent_name.split_once('/') {
        apply_package_agent_transforms(&mut agent_config, pkg, stem)?;
    }
    
    // 3. Resolve file-backed variable defaults
    resolve_file_backed_variables(agent_config.variables_mut(), &agent_dir)?;
    
    // 4. Set shared_variables for template interpolation
    agent_config.set_shared_variables(shared_variables);
    
    // 5. Expand use_tools via live MCP
    let active_pkg = pkg_from_qualified(agent_name);
    let expanded_tools = config.expand_use_tools(agent_config.use_tools(), active_pkg);
    
    // 6. Export rendered (interpolates body internally)
    agent_config.export_rendered(&expanded_tools)
}
```

Added `AgentConfig::export_rendered(expanded_tools)` in `crates/harnx-core/src/agent_config.rs`:

```rust
pub fn export_rendered(&self, expanded_tools: &[String]) -> Result<String> {
    let mut metadata = AgentFrontMatter::from_config(self);
    metadata.use_tools = Some(expanded_tools.to_vec());
    let body = self.interpolated_instructions()?;  // Interpolates templates
    // Assemble YAML front-matter + markdown body
}
```

### 2. Session Dump: `render_session_dump(agent_name, session_id)`

Located in `crates/harnx-runtime/src/config/session_dump.rs`:

```rust
pub fn render_session_dump(agent_name: Option<&str>, session_id: &str) -> Result<String> {
    let mut config = load_config_for_session_dump()?;  // MUST load from disk for clients
    let session_path = resolve_session_path(agent_name, session_id);
    // ... load session, render state-only YAML (model, tokens, messages, no system prompt)
}
```

### 3. CLI Commands

```bash
harnx info agent <name>   # Rendered agent-md, requires MCP init
harnx info session <agent> <id>  # State-only, NO MCP init
```

### 4. TUI Overlay Interception

`.info agent` and `.info session` intercepted in `input.rs` before command delegation, rendered into detail-view overlay (not transcript).

## Why This Works

**Pipeline ordering**: Patches run before interpolation because jaq filters may reference template variables that need final values. Interpolation happens in `export_rendered()` via `interpolated_instructions()`.

**MCP initialization**: `info agent` constructs minimal runtime (`Config::init` + `init_mcp_manager`) without a live session. `info session` deliberately skips MCP — it's state-only.

**Tool expansion**: `Config::expand_use_tools()` delegates to existing `tool_declarations_for_use_tools()`, preserving wildcard/toolset expansion and graceful MCP degradation (warn + continue on server failure).

**Session resolution**: `agent_name` selects storage root via `sessions_dir(agent_name)`, then `session_file(session_id)` handles `sub/leaf` ID paths.

**Config loading**: Session dump MUST load real config from disk (`Config::load_from_file`) to populate clients for model resolution. `Config::default()` has zero clients and fails model lookup.

## Prevention Strategies

**Test Cases:**
- `render_agent_dump_*` tests in `agent_tests.rs` covering file agents, builtin agents, package-qualified names
- `render_session_dump_*` tests covering agent-scoped and top-level sessions
- TUI overlay tests verify detail-view opens without transcript pollution

**Best Practices:**
- Single shared `render_agent_dump()` function for CLI and TUI — never duplicate
- `active_pkg` must use `pkg_from_qualified()`, not naive `split('/').next()` (returns `Some(name)` for non-package agents)
- Deterministic insta snapshots: avoid env-dependent template vars (`__now__`, `__cwd__`) in fixtures, or scrub via normalizers
- Verify `git status` before committing; never use `git add -A` blindly (delegates left scratchpad binaries)

**Code Review Checklist:**
- [ ] Does session dump load real config (not `Config::default()`)?
- [ ] Does tool expansion handle MCP failure gracefully?
- [ ] Are patches applied before interpolation?
- [ ] Does `active_pkg` return `None` for non-package agents?

## Related Issues

- **GitHub:** [#622](https://github.com/dobesv/harnx/issues/622) — Dump fully-rendered agent config
- **Related Solution:** [logic-errors/minijinja-system-prompt-templating-2026-04-25.md](../logic-errors/minijinja-system-prompt-templating-2026-04-25.md) — MiniJinja context construction
- **Related Solution:** [logic-errors/extract-agent-precedence-2026-05-03.md](../logic-errors/extract-agent-precedence-2026-05-03.md) — Agent config precedence
- **Related Solution:** [integration-issues/mcp-tool-template-acp-propagation-2026-04-30.md](../integration-issues/mcp-tool-template-acp-propagation-2026-04-30.md) — MCP tool template rendering
