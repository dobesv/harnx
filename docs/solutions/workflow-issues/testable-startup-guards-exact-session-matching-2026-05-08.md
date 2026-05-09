---
title: "Testable startup guards and exact session matching for CLI context reuse"
date: 2026-05-08
category: "workflow-issues"
problem_type: workflow_issue
component: "harnx-tui, harnx-runtime/config, harnx CLI"
root_cause: "Disk-dependent startup checks untestable; CLI session matching used priority sort instead of exact match"
resolution_type: code_fix
severity: medium
tags:
  - testing
  - cfg-test
  - session-management
  - cli
  - tui
  - agent-switching
plan_ref: "harnx-451-require-agent-session"
---

## Problem

TUI startup guard `list_assistant_agents()` reads from disk, making `init()` untestable. CLI session auto-selection used priority-sorted ranking designed for TUI pickers, causing ambiguity when user wanted exact context match. Switching agents saved session after agent field changed, writing to wrong directory.

## Symptoms

- No unit tests for TUI "no agents available" startup guard
- CLI created new session instead of resuming exact-match session
- Session files written to wrong agent directory after agent switch
- `init()` required agents directory populated even for unit tests

## Investigation Steps

1. TUI `init()` called `list_assistant_agents()` directly; no way to inject agent list
2. `sort_sessions_for_picker()` uses priority ranking: terminal ID match first, then branch, then remote — designed for TUI picker "best candidate" UX
3. CLI needs exact match: resume if all context fields match, else create fresh session
4. Reviewed `use_agent()` path: `self.agent = Some(agent)` happened before `self.session.save()`, but `save()` uses `sessions_dir()` which reads `self.agent`

## Root Cause

**Untestable startup guard:** Startup checks read from disk/env not populated in test environments. Without extraction, the guard logic could not be unit tested.

**Wrong matching algorithm:** `sort_sessions_for_picker()` ranks sessions by priority for interactive pickers. CLI non-interactive mode needs exact match (all criteria must match, not "best match").

**Agent-switch ordering bug:** `sessions_dir()` is computed from `self.agent`. If agent field changes before session save, session files written to wrong directory.

## Solution

### 1. Testable Startup Guard with `cfg!(test)` Bypass

Extract pure logic into `pub(crate)` function that accepts the disk-dependent value as parameter:

```rust
// crates/harnx-tui/src/lifecycle.rs
impl Tui {
    /// Check whether agents are available to start the TUI.
    /// Extracted so it can be called and tested independently.
    pub(crate) fn check_agents_available(config: &GlobalConfig, agents: &[String]) -> Result<()> {
        if config.read().agent.is_none() && agents.is_empty() {
            anyhow::bail!(
                "No agents configured. Create an agent file in the agents/ directory first."
            );
        }
        Ok(())
    }

    pub fn init(...) -> Result<Self> {
        let agents = list_assistant_agents();
        // Skip agents check in tests: test configs use bare Config::default()
        // with no agents directory, but check is covered by direct unit tests.
        if !cfg!(test) {
            Self::check_agents_available(config, &agents)?;
        }
        // ... rest of init
    }
}
```

Unit tests call `check_agents_available()` directly with controlled inputs:

```rust
// crates/harnx-tui/src/tests.rs
#[tokio::test]
async fn test_check_agents_available_errors_when_no_agent_and_no_agents() {
    let config = GlobalConfig::default();
    let result = Tui::check_agents_available(&config, &[]);
    assert!(result.is_err(), "must error when no agent and no agents");
}

#[tokio::test]
async fn test_check_agents_available_ok_when_agents_exist() {
    let config = GlobalConfig::default();
    let agents = vec!["assistant-agent".to_string()];
    let result = Tui::check_agents_available(&config, &agents);
    assert!(result.is_ok(), "must not error when agents are available");
}
```

### 2. Exact Match for CLI Session Auto-Selection

Add `find_matching_session()` that requires all non-None context fields to match:

```rust
// crates/harnx-runtime/src/config/session_meta.rs
/// For CLI auto-session: find a session that exactly matches all available
/// context fields. All non-None current context fields must match.
/// Returns the most recent matching session's id, or None if no match.
pub fn find_matching_session(
    sessions: &[SessionMeta],
    context: &PickerContext,
    agent_name: &str,
) -> Option<String> {
    let candidates: Vec<_> = sessions
        .iter()
        .filter(|s| {
            if s.agent_name.as_deref() != Some(agent_name) {
                return false;
            }
            if let Some(term_id) = &context.current_terminal_id {
                if s.terminal_session_id.as_deref() != Some(term_id.as_str()) {
                    return false;
                }
            }
            // ... same pattern for git_branch, git_remote, working_dir
            true
        })
        .collect();
    candidates.sort_by_key(|s| session_recency_key(s));
    candidates.first().map(|s| s.id.clone())
}
```

CLI entry calls this for context-aware session reuse:

```rust
// crates/harnx/src/main.rs, WorkingMode::Cmd branch
let context = build_picker_context();
let existing_session = find_matching_session(&sessions, &context, agent_name);
config.write().use_session(existing_session.as_deref())?;
```

**Key distinction:**
- `sort_sessions_for_picker()`: Priority-ranked for TUI interactive picker
- `find_matching_session()`: Exact match for CLI non-interactive reuse

### 3. Exit Session Before Changing Agent

`use_agent()` now calls `exit_agent()` before setting `self.agent`:

```rust
// crates/harnx-runtime/src/config/mod.rs
pub async fn use_agent(&mut self, agent_name: &str, ...) -> Result<()> {
    if config.read().agent.is_some() {
        config.write().exit_agent()?;  // Exit BEFORE agent field changes
    }
    let agent = init(config, agent_name, abort_signal).await?;
    // Now safe to activate new agent
}
```

`exit_agent()` calls `self.session.save()` which uses `sessions_dir()` computed from current `self.agent`. Order matters: save first, then change agent.

## Why This Works

1. **Testable extraction:** Pure function accepts disk-dependent value as parameter; unit tests verify logic without disk I/O
2. **`cfg!(test)` is legitimate here:** Bypass only acceptable because (a) check logic is separately unit tested, (b) comment explains why bypass exists
3. **Exact matching for CLI:** Non-interactive mode needs deterministic "resume or create fresh" behavior, not "pick best candidate"
4. **Exit-before-change ordering:** Session save path computed from agent field; must save before field mutation

## Prevention Strategies

**Test Cases:**
- `test_check_agents_available_errors_when_no_agent_and_no_agents` — guard fails with no agent configured
- `test_check_agents_available_ok_when_agents_exist` — guard passes when agents available
- `test_check_agents_available_ok_when_agent_selected` — guard passes when agent already active
- `test_find_matching_session_matches_all_available_context_fields` — exact match on all fields
- `test_find_matching_session_skips_none_context_fields` — None fields treated as wildcard

**Pattern Recognition:**
- `cfg!(test)` bypass is acceptable ONLY when bypassed logic is separately unit tested via extracted function
- Document the bypass with a comment explaining why it exists
- TUI picker uses priority sort; CLI non-interactive uses exact match

**Code Review Checklist:**
- [ ] Startup checks that read disk/env extracted to testable functions
- [ ] `cfg!(test)` bypasses have explanatory comments
- [ ] CLI session matching uses exact match, not priority sort
- [ ] Session save happens before agent/session field mutation
- [ ] Agent switching tests verify session files written to correct directory

## Key Learnings

1. **Testable startup guards:** Extract pure logic to `pub(crate)` function accepting disk-dependent value as parameter; use `cfg!(test)` in `init()` to bypass disk read; unit test extracted function directly
2. **Session-context matching for CLI:** Non-interactive CLI needs exact matching (all criteria must match); TUI picker needs priority-sorted ranking (best candidate at top)
3. **Agent-scoped session switching:** Save session BEFORE changing agent field; `sessions_dir()` computed from `self.agent`
4. **`cfg!(test)` bypass pattern:** Legitimate when (a) bypassed logic is separately unit tested, (b) documented with explanatory comment

## Related Issues

- **GitHub:** [#451](https://github.com/dobesv/harnx/issues/451) — Require agent and session for all activity
- **GitHub:** [#450](https://github.com/dobesv/harnx/issues/450) — Allow switching agent/session without .exit first
- **Related Solution:** [logic-errors/agent-switch-with-session-consistency-2026-05-03.md](../logic-errors/agent-switch-with-session-consistency-2026-05-03.md) — Agent switching session ordering
- **Related Solution:** [integration-issues/session-picker-multi-factor-sorting-2026-05-02.md](../integration-issues/session-picker-multi-factor-sorting-2026-05-02.md) — TUI picker priority sort
- **Related Solution:** [logic-errors/cli-session-auto-created-cmd-mode-2026-05-08.md](../logic-errors/cli-session-auto-created-cmd-mode-2026-05-08.md) — CLI session bootstrap
- **Files Changed:**
  - `crates/harnx/src/main.rs` — CLI agent guard + context-aware session reuse
  - `crates/harnx-runtime/src/config/mod.rs` — session/agent switching without forced exit, removed legacy save fallback
  - `crates/harnx-runtime/src/config/session_meta.rs` — `find_matching_session()` for exact CLI context match
  - `crates/harnx-tui/src/lifecycle.rs` — startup guard extracted to testable function
  - `crates/harnx-tui/src/tests.rs` — 3 new tests for `check_agents_available`
  - `crates/harnx-runtime/src/commands.rs` — removed `.exit session`/`.exit agent`
