---
title: "Session variable dot-commands: verb-first convention, 3-layer dispatch, and persistence path"
date: 2026-07-29
category: "feature-implementation"
problem_type: logic_error
component: "harnx-runtime, harnx-tui"
root_cause: "n/a — feature implementation pattern documentation"
resolution_type: code_fix
severity: medium
tags:
  - dot-commands
  - session-variables
  - tui-overlay
  - session-lock
  - verb-first
  - dispatch-routing
  - editor-integration
plan_ref: "session-variables-tui"
---

## Problem

Adding new dot-commands for session variable management required understanding three distinct architectural layers: (1) command registry, (2) CLI dispatch routing, and (3) TUI overlay intercept. Additionally, the variable mutation+persistence path needed to correctly coordinate SessionLock acquisition with `save_session` re-entrancy semantics.

## Symptoms

N/A — feature implementation.

## Key Architectural Patterns

### 1. Dot-Command Convention and 3-Layer Architecture

Harnx uses a **verb-first** naming convention for dot-commands (`.info session`, `.edit config`, `.set model`). When adding new commands, three layers must be updated:

1. **Registry** (`crates/harnx-runtime/src/commands.rs`):
   - Static array `COMMANDS: [Command; N]` holds help text entries
   - **N must be bumped manually** when adding commands (compile-time count)
   - Location: `commands.rs:33`

2. **CLI Dispatch** (`run_command_with_output` in `commands.rs`):
   - `match parse_command(line)` branches route to handler methods
   - Command-specific parsing (`starts_with`, `split_once`) lives inline
   - `.set` and `.load` use `split_once(' ')` to allow values with spaces

3. **TUI Overlay Intercept** (`harnx-tui/src/input.rs`):
   - `try_handle_info_overlay` handles `.info` subcommands that open modals
   - Disambiguation via boolean prefix checks (plural `.info variables` vs singular `.info variable`)
   - Calls `config.read().method()` directly, wraps result in overlay

**Example**: For `.info variable <name>`:
- Registry: `Command::new(".info variable <name>", "Show one session variable's full value")`
- Dispatch: `rest.starts_with("variable ")` → `config.read().get_variable(name)?`
- TUI: `is_info_variable` check → `config.read().get_variable(&tokens[2])` → `open_info_overlay`

### 2. Session Variable Mutation + Persistence Path

Variable mutation follows a specific sequence:

```rust
let lock = SessionLock::acquire(&self.session_file(&session_name))?;
let mut variables = session.agent_variables().clone();
variables.insert(name.to_string(), value.to_string());
agent.set_session_variables(variables);
session.sync_agent(agent)?;
drop(lock);  // save_session re-acquires this same non-reentrant lock
self.save_session(None)?;
```

**Key points**:

- `SessionLock` is file-based (`flock`) and **non-reentrant within a process**
- Must drop lock before `save_session` because `save_session` acquires it internally
- Pattern matches sibling modules (`settings_split.rs`, `session_ops_split.rs`)
- Background: [local-session-cross-process-locking-2026-07-17.md](../integration-issues/local-session-cross-process-locking-2026-07-17.md)

### 3. Truncation Reuse (`truncate_middle`)

The `compaction::truncate_middle(text, max_chars)` helper at `compaction.rs:63` is reused for:

- `.info variables` — bulk display truncated to 200 chars
- `.info session` TUI dump — agent_variables truncated in `session_dump.rs`
- `.info variable <name>` — **full value, no truncation**

**Budget**: 200 chars for bulk; distinction enforced at call site.

Import via `use super::compaction::truncate_middle;` in handler module.

### 4. `$EDITOR` Round-Trip Pattern

Temp file editing uses a consistent pattern (precedent: `session_ops_split.rs:278-297`):

```rust
let temp_file = if let Some(ref dir) = self.temp_dir_override {
    dir.join(format!("variable-edit-{}.txt", uuid::Uuid::new_v4()))
} else {
    temp_file("variable-edit", ".txt")
};
std::fs::write(&temp_file, current_value)?;

let edit_result = self.edit_with_tui_hooks(|this| {
    let editor = this.editor()?;
    edit_file(&editor, &temp_file)
});
let edited_content = std::fs::read_to_string(&temp_file);
let _ = std::fs::remove_file(&temp_file);  // best-effort cleanup
edit_result?;
let edited_content = edited_content?;

self.set_variable(name, &edited_content)?
```

**Gotcha**: `edit_with_tui_hooks` installed via `set_tui_editor_hooks` at TUI lifecycle (`lifecycle.rs`) to suspend/resume alt-screen.

### 5. Test Bootstrap for Variable Mutations

Variable mutation tests need **both** an active agent **and** a non-empty session:

```rust
fn variable_test_config(sessions_dir: PathBuf) -> Config {
    let mut config = editor_test_config(sessions_dir);
    let mut agent = Agent::new(AgentConfig::from_prompt(""));
    agent.set_model(config.model.clone());
    config.agent = Some(agent);
    config
}
```

Persistence tests must add at least one `Message` to `session.messages` before saving. `use_session` treats empty-sessions as new, triggering `init_agent_variables` which may reset variables from defaults.

**Fake editor pattern** (for testing `edit_variable`):

```rust
let _env_lock = ENV_MUTEX.lock().unwrap();  // serialize EDITOR manipulations
let original_editor = std::env::var("EDITOR").ok();
std::env::set_var("EDITOR", "true");  // no-op editor

let mut config = variable_test_config(temp.path().to_path_buf());
config.temp_dir_override = Some(temp.path().to_path_buf());
config.set_tui_editor_hooks(Arc::new(|| {}), Arc::new(|_success| {}));

// ... test code ...

if let Some(val) = original_editor {
    std::env::set_var("EDITOR", val);
} else {
    std::env::remove_var("EDITOR");
}
```

Reference: `session_edit_tests.rs` for complete examples.

### 6. TUI Help Snapshot Gotcha

Adding commands changes the help snapshot:

- File: `crates/harnx-tui/src/snapshots/harnx_tui__tests__help_in_tui.snap`
- Expect snapshot diff when adding new commands
- Column alignment may break if command names exceed hardcoded padding width (24 chars)

## Related Issues

- GitHub: [#1254](https://github.com/dobesv/harnx/issues/1254) — Session Variable Management via dot-commands
- GitHub: [#1163](https://github.com/dobesv/harnx/issues/1163) — Truncate large variable values in session dump

## Related Solutions

- [local-session-cross-process-locking-2026-07-17.md](../integration-issues/local-session-cross-process-locking-2026-07-17.md) — SessionLock and re-entrancy semantics
- [restored-session-agent-variable-defaults-2026-07-28.md](restored-session-agent-variable-defaults-2026-07-28.md) — Variable defaults on session restore
- [info-session-compact-summary-2026-05-22.md](info-session-compact-summary-2026-05-22.md) — CLI vs TUI info paths
- [dot-commands-picker-signals-2026-05-10.md](dot-commands-picker-signals-2026-05-10.md) — CommandOutcome for TUI modal signals
