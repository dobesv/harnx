---
title: "Session resume hint on exit in a Rust CLI"
date: 2026-05-05
category: logic-errors
problem_type: logic_error
component: cli-session-management
root_cause: "missing user-facing guidance after session exit"
resolution_type: code_fix
severity: low
tags:
  - session-management
  - cli-ux
  - shell-escaping
  - terminal-output
plan_ref: "issue-417"
---

## Problem

Users had no way to know how to resume a harnx session after exiting. No command or hint was printed, requiring users to manually recall agent and session names.

## Symptoms

- After exiting a harnx session (TUI or CMD mode), terminal returned to prompt with no guidance
- Users had to remember session names and agent to resume: `harnx -a <agent> -s <session>`
- Temp sessions (with timestamped filenames) were especially hard to resume — id shows as "temp" but file is timestamped

## Investigation Steps

1. Identified exit path in `exit_session()` which takes ownership of session via `.take()` — data gone after call
2. Found `session.resolved_save_name()` returns actual on-disk filename for temp sessions (not `"temp"`)
3. Determined hint must print BEFORE `exit_session()` consumes session state
4. Tested output destination: TUI restores terminal before this point, so stdout goes to TUI buffer
5. Added shell escaping for copy-paste safety — names may contain spaces or special characters

## Root Cause

Session exit logic provided no user-facing feedback. Session data was consumed by `exit_session()` before any hint could access it, and the distinction between `session.id()` (logical ID) and `resolved_save_name` (on-disk name) was not surfaced.

## Solution

Capture session state BEFORE calling `exit_session()`, print hint to stderr:

```rust
// Capture BEFORE exit_session() takes ownership
let session_name = session.resolved_save_name();
let agent_name = config.read().agent.as_ref().map(|a| a.name.clone());

// Suppress conditions
let is_empty_session = session.is_empty();
let save_disabled = save_session == Some(false) && !save_session_this_time;

if !is_empty_session && !save_disabled {
    eprintln!("\nResume this session by running:");
    let quoted_session = shell_words::quote(&session_name);
    if let Some(agent) = agent_name {
        let quoted_agent = shell_words::quote(&agent);
        eprintln!("  harnx -a {} -s {}", quoted_agent, quoted_session);
    } else {
        eprintln!("  harnx -s {}", quoted_session);
    }
}

// NOW safe to call exit_session()
exit_session(&config, save_session, save_session_this_time)?;
```

**Unit test pattern** — construct `Config::default()` with mock `Session`:

```rust
#[test]
fn test_resume_hint_empty_session_suppressed() {
    let mut config = Config::default();
    config.session = Some(Session::default()); // derives Default
    // assertion logic here
}
```

## Why This Works

1. **Capture before consume** — session state is read before `exit_session()` takes ownership via `.take()`
2. **`resolved_save_name` over `id()`** — temp sessions have id `"temp"` but save to timestamped files; `resolved_save_name()` returns actual on-disk name
3. **`shell_words::quote()`** — names are copy-paste commands; must handle spaces and special chars safely
4. **`eprintln!` to stderr** — TUI restores terminal before this point; stdout goes to TUI's alternate screen buffer, stderr goes to raw terminal
5. **Suppression logic** — avoid noise for empty sessions and explicitly-disabled save cases

## Prevention Strategies

**Best Practices:**

- Always capture data BEFORE ownership-transfer methods like `.take()` or `.remove()`
- Use `shell_words::quote()` for any user-facing shell commands
- Print CLI hints to stderr when TUI has restored terminal state
- Test mock objects by deriving `Default` — no filesystem needed

**Code Review Checklist:**

- [ ] Is session state captured before ownership transfer?
- [ ] Are copy-paste commands shell-safe?
- [ ] Does output go to correct stream (stderr vs stdout)?
- [ ] Are hints suppressed for edge cases (empty, disabled)?

**Related Tests:**

- Unit test with `Config::default()` and `Session::default()` — validates suppression logic
- Integration test: run harnx with temp session, exit, verify hint shows timestamped name

## Related Issues

- **Changeset:** `.changesets/417-session-resume-hint.md`
- **Related Solution:** [logic-errors/non-tui-terminal-output-fixes-2026-04-30.md](./non-tui-terminal-output-fixes-2026-04-30.md) — TUI vs non-TUI output handling
