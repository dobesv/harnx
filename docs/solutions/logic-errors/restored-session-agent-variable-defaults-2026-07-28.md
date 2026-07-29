---
title: "Restored sessions pick up new agent variable defaults"
date: 2026-07-28
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime config session restore"
root_cause: "restored session branch copied persisted variable snapshot verbatim without re-resolving defaults for newly-defined variables"
resolution_type: code_fix
severity: medium
tags:
  - session-restore
  - agent-variables
  - defaults
  - minijinja
  - strict-templating
plan_ref: "agent-var-defaults"
---

## Problem

When a user added a new `variables:` entry with a `default:` to an agent definition, restoring a prior session failed on startup with a strict-mode template render error ("variable not defined"). The restored session used its stale persisted variable snapshot without merging in defaults for variables that didn't exist when the session was saved.

## Symptoms

```
Error: Template error in agent 'my-agent': 'greeting' is undefined
```

- Occurred at session restore time (`use_session()`) after agent definition gained new variables
- Template rendering failed because MiniJinja runs in `UndefinedBehavior::Strict` mode
- Previously saved sessions became unusable after agent definition evolved
- No way to recover except manual session reset (wiping transcript) or `.edit session` to add missing keys

```
Behavior: Restoring session 'old-session' fails immediately after agent 'my-agent' gains variable 'greeting' with default 'hello'
Frequency: Reproducible whenever agent definition gains new variables between session save and restore
Impact: Users cannot resume work in existing sessions after agent upgrades
```

## Investigation Steps

Traced through `Config::init_agent_session_variables` (crates/harnx-runtime/src/config/mod.rs:1220-1267) and found two code paths:

1. **New session branch**: Builds merged map, calls `agent::init_agent_variables()` which fills defaults
2. **Restored session branch (else)**: Copied `session.agent_variables()` verbatim, skipping default resolution entirely

The root cause was asymmetric handling: new sessions resolved defaults but restored sessions did not. Template rendering uses MiniJinja `UndefinedBehavior::Strict` (crates/harnx-core/src/system_vars.rs), which only inserts keys present in `agent.variables()`. A missing newly-defined variable triggers a hard error.

Reviewed `agent::resolve_agent_variable` (crates/harnx-runtime/src/config/agent.rs:443-459) and confirmed it returns an existing value from the passed map before falling back to the default. This meant re-running resolution against a persisted map would preserve saved values while filling defaults for missing keys.

## Root Cause

`Config::init_agent_session_variables` had two branches:

- **New session**: Built merged map and called `agent::init_agent_variables(...)`, filling defaults for all defined variables
- **Restored session**: Copied `session.agent_variables()` directly to agent, never calling `init_agent_variables()`

Template rendering with `UndefinedBehavior::Strict` only includes keys from `agent.variables()`. When a new variable is added to an agent definition, restoring an old session left that variable missing, causing immediate template render failure.

## Solution

In the restored-session branch, re-resolve defined variables against the persisted map:

1. Clone persisted `session.agent_variables()` as the base map
2. Layer any `self.agent_variables` (CLI/config overrides) on top
3. Call `agent::init_agent_variables(agent.defined_variables(), &base, self.info_flag)`
4. Set resolved variables on agent via `set_shared_variables()` and `set_session_variables()`
5. Call `session.sync_agent(agent)` to re-persist the merged map

**Before:**

```rust
} else {
    let variables = session.agent_variables();
    agent.set_session_variables(variables.clone());
}
```

**After:**

```rust
} else if agent.defined_variables().is_empty() {
    // Pass-through for agents without defined variables
    let variables = session.agent_variables();
    agent.set_session_variables(variables.clone());
} else {
    // Re-resolve defined variables against persisted map to fill missing defaults
    let mut base_variables = session.agent_variables().clone();
    if let Some(config_variables) = &self.agent_variables {
        base_variables.extend(config_variables.clone());
    }
    let resolved_variables = self::agent::init_agent_variables(
        agent.defined_variables(),
        &base_variables,
        self.info_flag,
    )?;
    agent.set_shared_variables(resolved_variables.clone());
    agent.set_session_variables(resolved_variables);
    session.sync_agent(agent)?;
}
```

## Why This Works

`resolve_agent_variable` (agent.rs:449-453) returns an existing value from the passed map before checking the default. This guarantees:

- **Existing values are preserved**: Keys in `session.agent_variables()` take precedence over defaults
- **Missing keys get defaults**: Variables added to agent definition after session was saved pick up their `default:` values
- **Non-destructive**: User's previously saved variable values are never overwritten by defaults

The `sync_agent()` call re-renders the template on restore, ensuring the session state is consistent. If a variable added without a default is missing, failure occurs at restore time with a clear error, not deferred to first message-send.

## Non-Obvious Notes / Gotchas

1. **New failure path for required variables**: Variables added WITHOUT a default now fail fast at restore (via `sync_agent` template render) instead of at first message. A user can unblock via `--agent-variable key=value` CLI flag.

2. **Precedence**: CLI/config `--agent-variable` overrides win over persisted session values on restore. This is intentional — CLI flags are one-shot, per-process overrides. Currently no in-session variable-set mechanism exists to cause silent data loss.

3. **Pruning**: Restore now prunes persisted keys no longer in `agent.defined_variables()`. This is desirable cleanup of deprecated state.

4. **Workaround without code change**: Users can run `.reset session` (re-expands variables but wipes transcript) or `.edit session` to add missing keys to `agent_variables:` (lossless).

5. **Follow-up gaps**: NATS worker session restore (nats_worker/agent_loop.rs) and `use_agent_obj` with an active session don't run this re-resolution yet.

## Prevention Strategies

**Test Cases:**

- Test restoring a session when agent gains a new variable with a default — assert default is filled
- Test restoring a session when agent gains a variable that was previously saved — assert saved value preserved
- Test that resolved defaults are synced back to session storage (`session.agent_variables()`)

**Code Review Checklist:**

- [ ] New agent variable defaults are considered for backward compatibility with existing sessions
- [ ] Required variables (no default) documented clearly — users need CLI flag to restore old sessions
- [ ] Session restore paths (NATS, agent switch) reviewed for similar stale-snapshot issues

**Monitoring:**

- Log template render errors at session restore separately from at message-send time
- Track `use_session()` failures attributed to undefined variables

## Related Issues

- **Related Solution:** [minijinja-system-prompt-templating-2026-04-25.md](minijinja-system-prompt-templating-2026-04-25.md) — Established strict-mode templating and `UndefinedBehavior::Strict`
- **Plan:** agent-var-defaults
- **Test:** `test_restored_sessions_resolve_new_agent_variable_defaults` in crates/harnx-runtime/src/config/tests.rs:364-481
