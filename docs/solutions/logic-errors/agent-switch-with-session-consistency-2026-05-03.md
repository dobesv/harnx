---
title: "Agent switching with active session: exit_agent before activation"
date: 2026-05-03
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime/config"
root_cause: "Sync use_agent_by_name path did not call exit_agent before activating new agent, causing guard_empty error"
resolution_type: code_fix
severity: medium
tags:
  - session-management
  - agent-switching
  - state-consistency
plan_ref: "session-management-451-450-448-449"
---

## Problem

Switching agents when a non-empty session was active failed with "guard_empty" error. The sync code path (`use_agent_by_name`/`use_agent_obj`) did not call `exit_agent()` before activating the new agent, while the async path (`use_agent`) did. This inconsistency caused the guard check in `use_agent_obj` to fail.

## Symptoms

- Error: "Cannot perform this operation because you are in a non-empty session" when switching agents via TUI picker
- Agent switching worked via CLI but failed via TUI
- Switching agents with an empty session succeeded, but failed with messages in transcript

## Root Cause

Two code paths for agent activation had different behavior:

**Async path (correct):**
```rust
// crates/harnx-runtime/src/config/mod.rs
pub async fn use_agent(...) -> Result<()> {
    if config.read().agent.is_some() {
        bail!("Already in a agent, please run '.exit agent' first");
    }
    // ... load agent ...
    config.write().agent = Some(agent);
    if let Some(session) = session {
        config.write().exit_session()?;  // <-- Exits session before switching
        config.write().use_session(Some(&session))?;
    }
    Ok(())
}
```

**Sync path (missing exit):**
```rust
// BEFORE (bug)
pub fn use_agent_obj(&mut self, agent: Agent) -> Result<()> {
    if let Some(session) = self.session.as_mut() {
        session.guard_empty()?;  // <-- Fails if session has messages
        session.set_agent(&agent)?;
    } else {
        self.agent = Some(agent);
    }
    Ok(())
}
```

The `guard_empty()` check ensures you don't switch agents mid-session (which would orphan messages). But the intent of #450 was to allow direct switching — the check should be bypassed by exiting first.

## Solution

Add `exit_agent()` call at the start of `use_agent_obj` to match async path behavior:

```rust
// crates/harnx-runtime/src/config/mod.rs
pub fn use_agent_obj(&mut self, agent: Agent) -> Result<()> {
    // Exit the current agent/session first to allow direct switching.
    // This matches the async use_agent() behavior.
    if self.session.is_some() || self.agent.is_some() {
        self.exit_agent()?;  // <-- Added: clears session and agent cleanly
    }

    // Now safe to activate new agent
    self.agent = Some(agent);

    // Create fresh session for new agent
    self.use_session(None)?;

    Ok(())
}
```

Alternative (preserve existing session for new agent):

```rust
pub fn use_agent_obj(&mut self, agent: Agent) -> Result<()> {
    if let Some(session) = self.session.as_mut() {
        // Session exists: just update agent reference
        session.set_agent(&agent)?;
    } else {
        // No session: just set agent
        self.agent = Some(agent);
    }
    Ok(())
}
```

The chosen approach depends on product requirements:
- **Exit and create fresh**: Clean slate for new agent (current implementation)
- **Preserve session**: Continue current session with new agent

## Why This Works

1. **Consistency with async path**: Both paths now have same pre-conditions
2. **Clean state transition**: `exit_agent()` clears session, agent, and marks discontinuity
3. **No guard_empty failure**: Session is cleared before the check runs

## Prevention Strategies

**Test Cases:**

```rust
#[test]
fn test_switch_agent_with_non_empty_session() {
    let mut config = Config::default();

    // Activate agent A and add messages
    config.use_agent_by_name("agent-a")?;
    config.use_session(None)?;
    config.session.as_mut().unwrap().messages.push(/* ... */);

    // Switch to agent B — should succeed, not fail with guard_empty
    config.use_agent_by_name("agent-b")?;

    assert!(config.session.as_ref().unwrap().is_empty());  // Fresh session
    assert_eq!(config.agent.as_ref().unwrap().name(), "agent-b");
}
```

**Code Review Checklist:**

- [ ] Do all agent activation paths have consistent session/agent cleanup?
- [ ] Is there a sync/async code path divergence?
- [ ] Does the error message match the actual constraint (session not empty)?

## Related Issues

- **GitHub:** [Issue #450](https://github.com/dobesv/harnx/issues/450) — Allow switching agents/sessions without .exit first
- **Related Solution:** [logic-errors/extract-agent-precedence-2026-05-03.md](../logic-errors/extract-agent-precedence-2026-05-03.md) — Agent configuration precedence
