---
title: "Picker flow state continuity and modal restoration patterns"
date: 2026-05-03
category: "logic-errors"
problem_type: logic_error
component: "harnx-tui"
root_cause: "Multi-step picker flow lost initial state for reconciliation; modal dismissed before validating selection"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - modal-state
  - state-machine
  - error-handling
  - picker
plan_ref: "picker-bugs-435"
last_updated: 2026-05-05
---

## Problem

Four bugs in the agent/session picker flow caused incorrect behavior:
1. AgentPicker lacked text filter — users couldn't narrow long agent lists
2. SessionPicker ESC didn't create a new session — dismiss behavior was inconsistent
3. SessionPicker showed wrong sessions — `list_sessions_with_meta()` was called before agent activation, returning sessions from the previous agent's directory
4. AgentPicker ESC dismissed picker when no agent was active — mandatory picker enforcement broken
5. **(Issue #467)** ESC, Ctrl+C, Ctrl+D did nothing in pickers — no way to exit from picker modals

Underlying issue: multi-step picker flow (AgentPicker → SessionPicker) lost the initial agent/session state, causing transcript reconciliation to see `prev_agent == curr_agent` after an agent switch and fail to clear stale messages.

## Symptoms

- **Filter bug**: AgentPicker displayed all agents with no way to search
- **ESC bug**: Pressing ESC in SessionPicker did nothing (expected to start fresh session)
- **Wrong sessions bug**: Switching from agent A to agent B showed agent A's sessions in SessionPicker
- **Transcript bug**: Switching agents via picker left stale transcript messages from the previous agent
- **Mandatory picker bug**: Pressing ESC in AgentPicker when no agent was active dismissed the picker, leaving the app in an unusable state with no agent selected
- **No-exit bug (#467)**: ESC, Ctrl+C, and Ctrl+D did nothing in pickers — user had no way to abort the picker flow and exit the process

Error patterns from code review:
```rust
// Bug: modal taken before validation, never restored on invalid selection
let modal = self.app.modal.take();
match modal {
    Some(ModalState::AgentPicker { .. }) => {
        if selected >= filtered.len() {
            // Bug: falls through without restoring modal
        }
    }
}

// Bug: modal cleared before fallible operation
self.app.modal = None;
self.config.write().use_session(None)?;  // Error leaves UI inconsistent
```

## Investigation Steps

1. Traced `list_sessions_with_meta()` call in `resolve_initial_modal()` — found it reads `sessions_dir()` which is agent-scoped, but agent wasn't activated yet
2. Reviewed `SessionPicker` state — found `pending_agent` field was designed to defer activation, but ESC was redefined to mean "new session", making the deferral pointless
3. Analyzed transcript reconciliation flow — `reconcile_transcript_after_command()` compares `prev_agent`/`prev_session` to current values to detect transitions; if agent activated mid-flow, comparison sees no change
4. Identified modal restoration gap — `modal.take()` removes modal, then invalid selection path (empty filter) never restored it

## Root Cause

1. **Deferred activation was wrong abstraction**: `pending_agent` in SessionPicker tried to avoid "cancel leaves partial state", but ESC was redesigned to mean "new session" — no cancel path exists. Deferring activation caused `sessions_dir()` to return wrong directory.

2. **State not preserved across picker steps**: AgentPicker activated agent immediately, then SessionPicker captured `prev_agent` from current config (already the new agent). Reconciliation saw no agent transition.

3. **Modal dismissed before validation**: `modal.take()` followed by fallible operation with no restore path left UI in inconsistent state on error or invalid input.

## Solution

### Immediate Agent Activation with Origin Tracking

```rust
// crates/harnx-tui/src/types.rs
pub(super) enum ModalState {
    AgentPicker {
        agents: Vec<String>,
        selected: usize,
        query: String,  // Added: live filter text
    },
    SessionPicker {
        sessions: Vec<SessionMeta>,
        selected: usize,
        // Added: pre-activation state for reconciliation
        origin_agent: Option<String>,
        origin_session: Option<String>,
    },
}
```

Removed `pending_agent` from SessionPicker. Agent activates immediately on AgentPicker Enter, and `origin_agent`/`origin_session` carry the pre-activation baseline for reconciliation.

### Modal Restoration on Error

```rust
// crates/harnx-tui/src/input.rs — Enter handler
let modal = self.app.modal.take();
match modal {
    Some(ModalState::AgentPicker { agents, selected, query }) => {
        let filtered = ModalState::filtered_agents(&agents, &query);
        if selected >= filtered.len() {
            // Fix: restore modal on invalid selection
            self.app.modal = Some(ModalState::AgentPicker { agents, selected, query });
        } else {
            let agent_name = filtered[selected].to_string();
            let prev_agent = self.config.read().agent.as_ref().map(|a| a.name().to_string());
            let prev_session = self.config.read().session.as_ref().map(|s| s.id().to_string());

            if let Err(e) = self.config.write().use_agent_by_name(&agent_name) {
                // Fix: restore modal on error
                self.app.modal = Some(ModalState::AgentPicker { agents, selected: 0, query });
                return Err(e);
            }

            // Pass origin state to SessionPicker
            self.app.modal = Some(ModalState::SessionPicker {
                sessions: sorted,
                selected: 0,
                origin_agent: prev_agent,
                origin_session: prev_session,
            });
        }
    }
}
```

### ESC Creates New Session (with modal restore on error)

```rust
// crates/harnx-tui/src/input.rs — ESC handler
if let Some(ModalState::SessionPicker { origin_agent, origin_session, .. }) =
    self.app.modal.take()
{
    // Fix: only clear modal after success
    if let Err(e) = self.config.write().use_session(None) {
        self.app.modal = Some(ModalState::SessionPicker {
            sessions: vec![],
            selected: 0,
            origin_agent,
            origin_session,
        });
        return Err(e);
    }
    self.reconcile_transcript_after_command(origin_session, origin_agent, ".session");
} else {
    self.app.modal = None;
}
```

## Why This Works

1. **Immediate activation** ensures `sessions_dir()` returns the correct agent-scoped path when listing sessions
2. **origin_agent/origin_session** preserve the true pre-picker state so reconciliation detects the full agent+session transition, not just the session half
3. **Modal restoration on error** prevents inconsistent UI state where modal is dismissed but operation failed
4. **Validation before dismiss** keeps picker open when filter yields empty list (user can adjust query)

## Prevention Strategies

**Test Patterns:**

```rust
// RAII env var guard with mutex for serializing HARNX_CONFIG_DIR mutations
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvGuard { key: &'static str, prior: Option<String> }

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

// Usage in test:
let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner()); // poison recovery
let _env = EnvGuard::set("HARNX_CONFIG_DIR", tmp.path().to_str().unwrap());
```

Key test coverage:
- `agent_picker_down_bounded_by_filtered_count` — navigation respects filter
- `agent_picker_enter_on_empty_filter_does_nothing` — modal stays open
- `session_picker_esc_creates_new_session` — ESC triggers new session
- Sentinel transcript items to verify reconciliation clears stale state

**Code Review Checklist:**
- [ ] Does `modal.take()` have a restore path for every error/invalid branch?
- [ ] Are fallible operations called before clearing modal state?
- [ ] Does multi-step state machine preserve origin values for final reconciliation?
- [ ] Are session YAML stubs using old format (comments, not `retrieve_model` calls) to avoid validation failures?
- [ ] For mandatory pickers: does ESC check `config.read().agent.is_some()` before dismissing?

**Key Learnings:**
- `sessions_dir()` is agent-scoped; must activate agent before listing sessions
- `unwrap_or_else(|e| e.into_inner())` recovers from mutex poison in tests
- Multi-step flows need origin tracking for correct before/after comparison
- Redesigned semantics (ESC = new session) can make previous defensive patterns (deferred activation) into bugs
- **Mandatory picker enforcement**: For pickers shown at startup (no agent), ESC must NOT dismiss the picker. Check `config.read().agent.is_some()` before setting `self.app.modal = None`
- **(#467) Ctrl+C/Ctrl+D early-return pattern**: When handling Ctrl key combinations in a function that already has a `match key.code` block, use `if` guards before the match rather than converting to `match (key.code, key.modifiers)`. This avoids rewriting existing match arms that check `key.modifiers == KeyModifiers::NONE || KeyModifiers::SHIFT`.
- **(#467) Origin-based picker navigation**: Use `origin_agent` to distinguish "came from AgentPicker during startup" (`origin_agent.is_none()`) from "direct `-a <agent>` startup" (`origin_agent.is_some()`). In the former case, ESC goes back to AgentPicker; in the latter, ESC exits.
- **(#467) `should_quit` for mandatory picker exit**: For mandatory pickers at startup, ESC now sets `should_quit = true` (process exit) instead of being a no-op. Issue #451's "mandatory selection" was overridden — users can now abort the flow.

### Mandatory Picker ESC Handling

When agent+session are mandatory, ESC on AgentPicker must be blocked if no agent is active:

```rust
// crates/harnx-tui/src/input.rs
KeyCode::Esc => {
    // ESC on SessionPicker creates new session (agent already active)
    if let Some(ModalState::SessionPicker { .. }) = self.app.modal.take() {
        self.config.write().use_session(None)?;
        // ... reconcile transcript
    } else if let Some(ModalState::AgentPicker { .. }) = self.app.modal.take() {
        // Only dismiss AgentPicker if an agent is already active (user is switching)
        // If no agent active, picker is mandatory — ignore ESC
        if self.config.read().agent.is_some() {
            self.app.modal = None;
        } else {
            // Restore picker — user must select an agent
            self.app.modal = Some(ModalState::AgentPicker { ... });
        }
    }
}
```

### Ctrl+C/Ctrl+D Exit Handling (Issue #467)

In picker state, Ctrl+C and Ctrl+D should exit the process rather than being ignored. The prompt is not running during picker display, so there's nothing to abort — the only sensible action is process exit.

**Pattern: Early-return Ctrl checks before `match key.code`:**

```rust
// crates/harnx-tui/src/input.rs — handle_picker_key()
async fn handle_picker_key(&mut self, key: KeyEvent) -> Result<()> {
    // Ctrl+D always exits — no prompt running in picker state.
    if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
        self.app.should_quit = true;
        return Ok(());
    }
    // Ctrl+C exits in picker state — there is no in-flight prompt to abort.
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        self.app.should_quit = true;
        return Ok(());
    }
    match key.code {
        // ... existing match arms ...
    }
}
```

**Why early-return `if` guards?** Converting to `match (key.code, key.modifiers)` would require rewriting existing arms like `KeyCode::Char(c) if key.modifiers == KeyModifiers::NONE || KeyModifiers::SHIFT`. The `if` guard pattern keeps the change minimal and localized.

### Origin-Based ESC Routing (Issue #467)

SessionPicker ESC behavior depends on `origin_agent` and `origin_session`:

```rust
KeyCode::Esc => {
    if let Some(ModalState::SessionPicker { origin_session, origin_agent, .. }) =
        self.app.modal.as_ref()
    {
        if origin_session.is_some() {
            // Mid-switch cancel: restore original agent+session
            should_restore_origin = true;
        } else if origin_agent.is_none() {
            // Came from AgentPicker during startup (harnx → pick agent → session picker).
            // ESC goes back to AgentPicker so user can choose a different agent.
            let agents = list_assistant_agents();
            self.app.modal = Some(ModalState::AgentPicker {
                agents,
                selected: 0,
                query: String::new(),
            });
        } else {
            // Direct startup with `-a <agent>`, no prior session. ESC exits.
            self.app.should_quit = true;
        }
    }
}
```

**Key distinction:** `origin_agent.is_none()` means "came from AgentPicker" (the agent was picked in this flow, not pre-existing). `origin_agent.is_some()` means direct `-a <agent>` startup where the user explicitly specified the agent.

## Related Issues

- **PR:** [#435](https://github.com/dobesv/harnx/pull/435) — Agent session revamp
- **Issue:** [#467](https://github.com/dobesv/harnx/issues/467) — ESC/Ctrl-C/Ctrl-D in pickers
- **Related Solution:** [integration-issues/session-picker-multi-factor-sorting-2026-05-02.md](../integration-issues/session-picker-multi-factor-sorting-2026-05-02.md) — Original picker implementation
- **Related Solution:** [integration-issues/raw-mode-ctrl-c-interrupt-2026-04-30.md](../integration-issues/raw-mode-ctrl-c-interrupt-2026-04-30.md) — Raw-mode terminal interrupt handling (Ctrl-C/Ctrl-D in CLI one-shot mode)
