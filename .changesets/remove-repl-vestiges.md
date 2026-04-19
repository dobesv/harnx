---
harnx: minor
---
Removed vestiges of the old readline-based REPL. Harnx has two modes: interactive **TUI** (ratatui) and non-interactive **CLI**. There is no separate REPL. This change:

- Deletes the dead `src/repl/` reedline-based interactive loop, completer, highlighter, prompt renderer, and validator, along with the `reedline` dependency and the now-unused `left_prompt`/`right_prompt` config fields, `{left,right}_prompt` env vars, and `AssertState` machinery.
- Renames the `repl` module to `commands`, `ReplCommand` → `Command`, `REPL_COMMANDS` → `COMMANDS`, `run_repl_command*` → `run_command*`, `repl_complete` → `command_complete`, and `WorkingMode::Repl` → `WorkingMode::Tui` (with `is_repl()` → `is_tui()`).
- Renames the `repl_default_session` config key to `tui_default_session`. `repl_default_session` is still accepted as an alias in both YAML and `HARNX_REPL_DEFAULT_SESSION` for backward compatibility.
- Updates user-facing help text and documentation (`README.md`, `AGENTS.md`, `docs/tui-guide.md` — formerly `chat-repl-guide.md` — `configuration-guide.md`, `agent-guide.md`, `macro-guide.md`, `command-line-guide.md`). The `docs/custom-repl-prompt.md` guide is removed because the custom reedline prompt it described no longer exists. The `SessionStart` hook now reports `source: "tui"` instead of `"repl"` when the interactive UI starts. Closes #295.
