---
title: "CLI session auto-creation in non-interactive Cmd mode"
date: 2026-05-08
category: logic-errors
problem_type: logic_error
component: cli-session-management
root_cause: "missing session bootstrap in non-interactive CLI path"
resolution_type: code_fix
severity: medium
tags:
  - session-management
  - cli
  - non-interactive
  - streaming
  - agent-source
plan_ref: harnx-497-cli-session-resume
---

## Problem

Non-interactive CLI invocations (`harnx "prompt"`) never created a session. No transcript was saved, no session ID was assigned, and no resume hint was printed on exit. Users lost all conversation history from one-off CLI runs.

## Symptoms

- Running `harnx "explain this code"` produced output but no session file
- No resume hint printed after exit
- Session-related features (transcript logging, session resume) unavailable in Cmd mode
- Users unaware that their conversation was not being persisted

## Investigation Steps

1. Traced CLI entry point in `crates/harnx/src/main.rs` — `WorkingMode::Cmd` branch lacked session initialization
2. Found `install_cli_agent_event_sink()` called before `create_input()` — session bootstrap must go between these two
3. Identified `cfg.use_session(None)` as the correct call — invokes `new_anonymous_session_id()` → `session::new()` which captures `working_dir` and `terminal_session_id` automatically
4. Verified streaming path needed `AgentSource` on `Turn::Started` for CLI sink heading to display correctly

## Root Cause

Two related issues:

1. **Missing session bootstrap:** The `WorkingMode::Cmd` path never called `cfg.use_session(None)`, so `config.session` remained `None` throughout the CLI run. Without a session, no transcript was saved and no resume hint could be generated.

2. **Missing AgentSource on Turn::Started:** The streaming path in `call_chat_completions_streaming()` emitted `Turn::Started` without source info. The CLI sink (`CliAgentEventSink`) delays the source heading until first model output (chunk/completion), but `Turn::Started` is the canonical place to set `last_ui_output_source` via `maybe_emit_source_heading()`.

## Solution

### 1. Session Auto-Creation in Cmd Mode

Added session bootstrap after sink installation, before `create_input()`:

```rust
// crates/harnx/src/main.rs, WorkingMode::Cmd branch

let sink = agent_event_sink::install_cli_agent_event_sink(
    config.clone(),
    render_options,
    abort_signal.clone(),
);

// NEW: Auto-create session if none exists
{
    let mut cfg = config.write();
    if cfg.session.is_none() {
        cfg.use_session(None)?;
    }
}

let input = create_input(&config, text, &cli.file, abort_signal.clone()).await?;
```

Single write lock, minimal scope. `use_session(None)` calls `new_anonymous_session_id()` which generates a UUID session ID and captures context.

### 2. AgentSource on Turn::Started in Streaming Path

Added source extraction to `Turn::Started` emission:

```rust
// crates/harnx-runtime/src/client/common.rs, call_chat_completions_streaming()

use harnx_core::event::{AgentEvent, AgentSource, TurnEvent};
let agent_source = {
    let cfg = config.read();
    let agent = cfg.extract_agent().name().to_string();
    let session_id = cfg.session.as_ref().map(|s| s.id().to_string());
    AgentSource { agent, session_id }
};
harnx_core::sink::emit_agent_event_with_source(
    AgentEvent::Turn(TurnEvent::Started),
    Some(agent_source),
);
```

Matches existing pattern from `agent_loop.rs` for agent handoff/resume.

## Why This Works

**Session Bootstrap:** Placing session creation after sink installation but before `create_input()` ensures that all downstream code sees a valid session. The single write lock minimizes contention. Anonymous session IDs are UUID-based, avoiding collisions.

**AgentSource Propagation:** The CLI sink stores the source from `Turn::Started` in `last_ui_output_source`. When the first model output arrives, the heading is printed using this stored source. Without setting it on `Turn::Started`, the heading would be missing or incorrect.

## Prevention Strategies

**Test Coverage:**
- Test `session_resume_command()` includes both agent and session when agent is set (test: `returns_agent_and_session_in_resume_command`)
- Test that anonymous sessions with UUID IDs serialize correctly
- Test session auto-creation in Cmd mode by asserting `config.session.is_some()` after CLI entry

**Code Review Checklist:**
- [ ] All `WorkingMode` branches create or load a session
- [ ] `Turn::Started` events include `AgentSource` in streaming paths
- [ ] Session bootstrap happens before any session-dependent code

## Related Issues

- **GitHub:** [#497](https://github.com/dobesv/harnx/issues/497) — CLI runs should always create sessions
- **Related Solution:** [logic-errors/session-resume-hint-on-exit-2026-05-05.md](session-resume-hint-on-exit-2026-05-05.md) — Resume hint printing after exit
- **Related Solution:** [logic-errors/non-tui-terminal-output-fixes-2026-04-30.md](non-tui-terminal-output-fixes-2026-04-30.md) — AgentSource tracking in CLI sink
