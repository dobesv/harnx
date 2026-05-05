---
title: "Agent role field for filtering non-assistant agents from picker and completions"
date: 2026-05-04
category: "logic-errors"
problem_type: logic_error
component: "harnx-core, harnx-runtime, completions"
root_cause: "No mechanism to distinguish user-facing agents from internal subagents and compaction agents"
resolution_type: code_fix
severity: medium
tags:
  - agent-configuration
  - shell-completions
  - yaml-frontmatter
  - serde-default
  - zsh
plan_ref: "gh-457-agent-role-metadata"
---

## Problem

All agents appeared in the TUI agent picker and shell completions regardless of their intended purpose. Subagents (used internally by the orchestrator) and compaction agents (used for context compression) cluttered the UI and created confusing UX for end users who only need to select primary assistant agents.

## Symptoms

- Agent picker displayed internal subagents alongside user-facing agents
- Shell tab-completion suggested non-interactive agents (compaction, subagents)
- No way to categorize agents by role in the configuration system

## Investigation Steps

Reviewed existing `list_agents()` function — fast directory scan returning all `.md` filenames without parsing. Considered adding a `hidden` boolean field but recognized the need for explicit role categorization. Examined `MessageRole` enum pattern in `message.rs` as precedent for serde-based enum with defaults.

Evaluated adding filter logic directly to `list_agents()` but decided against it because:
1. Fast dir scan is valuable for other use cases
2. Filtering requires parsing (O(N) file reads)
3. Two-function pattern preserves backward compatibility

## Root Cause

Agent frontmatter had no `role` field. All agents were treated identically in UI contexts. The system lacked metadata to distinguish user-facing "assistant" agents from internal "subagent" and "compaction" agents.

## Solution

Added `AgentRole` enum and `list_assistant_agents()` function:

**AgentRole enum (harnx-core/src/agent_config.rs):**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    #[default]
    Assistant,
    Subagent,
    Compaction,
}
```

**Struct-level serde(default) on AgentFrontMatter:**

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]  // ← enables adding fields without breaking existing YAML
struct AgentFrontMatter {
    // ... other fields ...
    #[serde(default)]
    role: AgentRole,
}
```

**Two-function pattern (harnx-runtime/src/config/agent.rs):**

```rust
// Fast dir scan, no parsing — unchanged for backward compat
pub fn list_agents() -> Vec<String> { /* ... */ }

// Parses each file, filters by role
pub fn list_assistant_agents() -> Vec<String> {
    // ... reads each .md file, parses AgentConfig, filters by role == Assistant
}
```

**Zsh completion special case:**

```zsh
case $state in
    models|sessions|rags|macros)
        values=( ${(f)"$(_call_program values harnx --list-$state)"} )
        _wanted values expl $state compadd -a values && ret=0 ;;
    agents)  # ← must break pattern: flag is --list-assistant-agents, not --list-agents
        values=( ${(f)"$(_call_program values harnx --list-assistant-agents)"} )
        _wanted values expl $state compadd -a values && ret=0 ;;
esac
```

Other shell completions (bash, fish, nu, ps1) updated to call `--list-assistant-agents`.

## Why This Works

1. **Serde default semantics**: `#[serde(default)]` at struct level means YAML files without `role:` field deserialize successfully with `AgentRole::Assistant` as the implicit default. Existing agent configs require no modification.

2. **Enum rename_all**: `#[serde(rename_all = "snake_case")]` maps YAML `role: subagent` to `AgentRole::Subagent` automatically.

3. **Two-function pattern**: `list_agents()` remains fast (dir scan only) for code paths that don't need filtering. `list_assistant_agents()` pays the parsing cost only when preparing UI lists — infrequent, user-triggered operations.

4. **Zsh pattern break**: The zsh dynamic `--list-$state` pattern works for flags matching the state name. Since `--list-assistant-agents` doesn't match state `agents`, an explicit case arm is required.

## Prevention Strategies

**Test coverage:**
- Agent role parsing (default, each variant, roundtrip)
- `list_assistant_agents()` filtering (only assistants returned, malformed files skipped, sorted output)

**Code review checklist:**
- [ ] New YAML frontmatter fields include `#[serde(default)]` if they have `Default` impls
- [ ] Shell completion scripts updated when adding new `--list-*` flags
- [ ] Zsh state machine requires explicit case arm if flag name diverges from `--list-$state` pattern

**Performance consideration:**
If agent count grows significantly (hundreds of agents), `list_assistant_agents()` could become slow. Future optimization: frontmatter-only parser that doesn't parse the full markdown body.

## Related Issues

- **Jira:** [GH-457](https://github.com/dobesv/harnx/issues/457) — Add agent role metadata
- **Related Solution:** [logic-errors/extract-agent-precedence-2026-05-03.md](extract-agent-precedence-2026-05-03.md) — Agent configuration precedence rules
