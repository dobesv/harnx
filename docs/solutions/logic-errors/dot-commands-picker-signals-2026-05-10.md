---
title: "Runtime signals for TUI picker modals via CommandOutcome"
date: 2026-05-10
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime, harnx-tui"
root_cause: "No mechanism for runtime to request TUI modal opening; bare .agent/.session commands had unclear behavior"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - modal-state
  - command-handling
  - picker
  - runtime-tui-contract
plan_ref: "harnx-505-506"
---

## Problem

Two issues in `.agent`/`.session` dot-command handling:

1. **#505**: Switching agents via `.agent <agentname>` (no session arg) left user in no-session state — transcript showed no messages, subsequent commands had no session context.

2. **#506**: Bare `.agent` (0 args) showed usage text; bare `.session` did nothing. TAB on `.agent ` or `.session ` triggered text completions instead of opening their respective picker modals.

## Symptoms

- Running `.agent claude` successfully switched agent but left `session == None`
- `.agent` alone printed usage message (unhelpful)
- `.session` alone called `use_session(None)` which was a no-op
- TAB after `.agent ` or `.session ` would list completions rather than opening picker
- Inconsistent UX compared to startup picker flow

## Investigation Steps

1. Traced `.agent` command handler in `commands.rs` — found `None` case only wrote usage text
2. Traced `.session` handler — found `args.is_none()` case fell through to `use_session(None)`
3. Reviewed TUI's `handle_tab()` — always called `compute_completions();` no picker-aware logic
4. Noted existing picker opening: `open_agent_picker()`, `open_session_picker()` helpers existed but were only called from startup flow
5. Identified pattern: runtime needs way to signal TUI behavior beyond Continue/Exit

## Root Cause

No communication mechanism existed for runtime to request TUI modal behavior. The `CommandOutcome` enum only had `Continue` and `Exit` variants. Bare commands couldn't express "open the agent picker" or "open the session picker" as outcomes.

Additionally, TAB completion had no awareness of picker-commands — it always computed text completions, even when the user expected a modal picker.

## Solution

### 1. Extended CommandOutcome enum

Added two new variants to `CommandOutcome` in `crates/harnx-runtime/src/commands.rs`:

```rust
pub enum CommandOutcome {
    Continue,
    Exit,
    OpenAgentPicker,    // NEW
    OpenSessionPicker,  // NEW
}
```

### 2. Runtime returns picker outcomes

Modified `.agent` and `.session` handlers to return appropriate outcomes:

```rust
// .session with no args → open session picker
".session" => {
    if args.is_none() {
        return Ok(CommandOutcome::OpenSessionPicker);
    }
    config.write().use_session(args)?;
}

// .agent with no args → open agent picker
".agent" => match split_first_arg(args) {
    Some((agent_name, args)) => {
        // ... existing agent switching logic ...
    }
    None => return Ok(CommandOutcome::OpenAgentPicker),
},
```

### 3. TUI wires outcomes to modal opening

In `run_command()`, added handling for new outcomes:

```rust
match outcome {
    CommandOutcome::OpenAgentPicker => {
        self.open_agent_picker();
    }
    CommandOutcome::OpenSessionPicker => {
        self.open_session_picker();
    }
    // ... existing handlers ...
}
```

### 4. Agent switch triggers session picker when needed

Added logic to open SessionPicker after agent switch if no session selected:

```rust
// After successful .agent <name> command
let curr_agent = cfg.agent.as_ref().map(|a| a.name().to_string());
let session_missing = cfg.session.is_none();
drop(cfg);
if prev_agent != curr_agent && session_missing {
    self.open_session_picker();
}
```

### 5. TAB-to-picker detection

Added `picker_command_for_input()` helper in `crates/harnx-tui/src/input.rs`:

```rust
fn picker_command_for_input(line: &str, pos: usize) -> Option<PickerCommand> {
    let upto_cursor = &line[..pos];
    let trimmed = upto_cursor.trim_start();
    match trimmed.trim_end() {
        ".agent" => Some(PickerCommand::Agent),
        ".session" => Some(PickerCommand::Session),
        _ => None,
    }
}
```

Modified `handle_tab()` to check for picker commands when completions empty:

```rust
let picker_command = picker_command_for_input(&line, pos);
let completions = self.compute_completions(&line, pos).await;
if completions.is_empty() {
    match picker_command {
        Some(PickerCommand::Agent) => {
            self.open_agent_picker();
            return;
        }
        Some(PickerCommand::Session) => {
            self.open_session_picker();
            return;
        }
        None => return,
    }
}
```

### 6. Suppress text completions for picker-commands

In `compute_completions()`, return empty when detecting bare picker command:

```rust
// Suppress completions for bare .agent/.session commands
if matches!(cmd, ".agent" | ".session") && args.iter().all(|arg| arg.is_empty()) {
    return vec![];
}
```

The `args.iter().all(|arg| arg.is_empty())` check correctly matches both:
- `.agent` (no space after command, parsed as 0 args with empty first arg)
- `.agent ` (trailing space, parsed as 1 empty arg)

## Why This Works

`CommandOutcome` provides clean separation: runtime signals intent, TUI decides implementation. This avoids the runtime directly manipulating UI state (which it shouldn't know about).

The `picker_command_for_input()` helper normalizes whitespace the same way the command parser does (`trim_start()` + `trim_end()`), ensuring consistent detection of bare commands.

Suppressing completions for picker-commands ensures `handle_tab()` receives empty completions, triggering the picker-opening branch.

## Prevention Strategies

**Test Cases:**
- Verify `.agent` alone opens AgentPicker modal
- Verify `.session` alone opens SessionPicker modal
- Verify `.agent <name>` with no session opens SessionPicker after switch
- Verify TAB on `.agent ` opens picker (not completions)
- Verify TAB on `.agent cl` shows completions (not picker)

**Best Practices:**
- Use `CommandOutcome` variants for runtime → TUI communication; never call TUI methods from runtime
- Centralize modal-opening logic in TUI helpers (`open_agent_picker()`, `open_session_picker()`)
- Keep completion suppression logic synchronized with picker detection (`args.iter().all(|arg| arg.is_empty())`)

## Related Issues

- **Issue:** #505 — `.agent <agent>` leaves no session
- **Issue:** #506 — `.agent`/`.session` alone should open pickers
- **Related Solution:** [picker-flow-state-continuity-2026-05-03.md](picker-flow-state-continuity-2026-05-03.md) — AgentPicker/SessionPicker modal state management
- **Related Solution:** [session-picker-multi-factor-sorting-2026-05-02.md](../integration-issues/session-picker-multi-factor-sorting-2026-05-02.md) — SessionPicker sorting and metadata
