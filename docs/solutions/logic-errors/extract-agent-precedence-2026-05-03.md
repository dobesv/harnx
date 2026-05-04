---
title: "extract_agent precedence: self.agent over session-derived agent"
date: 2026-05-03
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime/config"
root_cause: "extract_agent preferred session-derived agent over in-memory agent, losing retry settings and hooks not stored in session log"
resolution_type: code_fix
severity: medium
tags:
  - session-management
  - agent-configuration
  - state-precedence
plan_ref: "session-management-451-450-448-449"
---

## Problem

When a user had both `self.agent` and `self.session` set simultaneously, `extract_agent()` returned the session-derived agent instead of the in-memory agent. This caused retry delays, hooks, and other agent configuration to revert to defaults when TUI sessions were active.

## Symptoms

- Retry settings (e.g., `retry_delay`, `max_retries`) ignored during TUI sessions
- Agent hooks not firing after session restore
- Configuration from agent YAML file lost after loading a session

## Root Cause

The session only stores a subset of agent config:
- Agent name
- Model ID
- Temperature, top_p
- System prompt

Not stored in session:
- Retry settings (`retry_delay`, `max_retries`, `retry_on_rate_limit`)
- Hooks (`pre_session`, `post_session`, etc.)
- Tool configurations
- Templates

Old `extract_agent` implementation checked session first:

```rust
// BEFORE (incorrect order)
pub fn extract_agent(&self) -> Agent {
    if let Some(session) = self.session.as_ref() {
        self::session::to_agent(session)  // Loses retry, hooks, etc.
    } else if let Some(agent) = self.agent.as_ref() {
        agent.clone()
    } else {
        // fallback
    }
}
```

When both are set, session-derived agent won, losing the rich configuration from the in-memory agent.

## Solution

Prefer `self.agent` over session-derived agent. The in-memory agent is authoritative because it has the full configuration loaded from the agent YAML file.

```rust
// crates/harnx-runtime/src/config/mod.rs
pub fn extract_agent(&self) -> Agent {
    // When an explicit agent is active, prefer it over the session-derived
    // agent. The in-memory agent has the full configuration from the agent
    // file (including retry settings, hooks, etc.) that may not be stored
    // in the session log. The session-derived agent is used only when
    // loading a standalone session from disk with no agent in context.
    if let Some(agent) = self.agent.as_ref() {
        agent.clone()
    } else if let Some(session) = self.session.as_ref() {
        self::session::to_agent(session)
    } else {
        // Fallback: create minimal agent from global config
        let mut agent = Agent::new(AgentConfig::from_prompt(""));
        agent.set_model(self.model.clone());
        agent.set_temperature(self.temperature);
        agent.set_top_p(self.top_p);
        agent.set_use_tools(self.use_tools.clone());
        agent
    }
}
```

## Why This Works

1. **In-memory agent is authoritative**: When the user activates an agent (via CLI flag, TUI picker, or `.agent` command), the full config is loaded from the agent YAML file. This is the intended configuration.

2. **Session-derived agent is fallback**: Only use the session-derived agent when loading a standalone session file without an active agent context (e.g., `harnx -s session.yaml` without `-a agent`).

3. **No config loss**: Retry settings, hooks, templates, and other agent-level config stay intact during TUI sessions.

## Prevention Strategies

**Test Cases:**

```rust
#[test]
fn test_extract_agent_prefers_in_memory_agent() {
    let mut config = Config::default();

    // Set up in-memory agent with custom retry
    let mut agent = Agent::new(AgentConfig::from_prompt("test"));
    agent.set_retry_delay(Some(std::time::Duration::from_secs(10)));
    config.agent = Some(agent);

    // Set up session with different model
    let mut session = Session::default();
    session.model_id = Some("different-model".to_string());
    config.session = Some(session);

    let extracted = config.extract_agent();

    // Should have in-memory agent's retry, not session's model
    assert_eq!(extracted.retry_delay(), Some(std::time::Duration::from_secs(10)));
}
```

**Code Review Checklist:**

- [ ] When both `self.agent` and `self.session` exist, does `extract_agent` prefer `self.agent`?
- [ ] Are agent-level configs (retry, hooks) tested after session activation?
- [ ] Is there documentation about what is/isn't stored in session files?

## Related Issues

- **GitHub:** [Issue #450](https://github.com/dobesv/harnx/issues/450) — Allow switching agents/sessions without .exit first
- **Related Solution:** [integration-issues/session-picker-multi-factor-sorting-2026-05-02.md](../integration-issues/session-picker-multi-factor-sorting-2026-05-02.md) — Session management context
