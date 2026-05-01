---
title: "Non-interactive Safe Defaults for Bash Child Processes"
date: 2026-04-30
category: integration-issues
problem_type: integration_issue
component: harnx-mcp-bash
root_cause: "Child processes could bypass stdin redirection via /dev/tty, causing TUI corruption, pager hangs, and ANSI escape clutter"
resolution_type: code_fix
severity: high
tags:
  - tty
  - credentials
  - pagers
  - environment-variables
  - non-interactive
  - git
  - ssh
plan_ref: fix-tty-writes-374
---

## Problem

Tools like `git` open `/dev/tty` directly for credential prompts, bypassing `stdin(Stdio::null())` and writing directly to the terminal/TUI. Interactive pagers (`less`, `more`) hang waiting for input that never arrives. Color-capable `$TERM` settings cause ANSI escape sequences that clutter LLM tool output.

## Symptoms

- "Username for 'https://github.com':" prompts appear in TUI, corrupting display
- Commands using `git log`, `man`, `journalctl` hang indefinitely (pager waiting for keystrokes)
- Tool output contains ANSI escape sequences (e.g., `[?1h`, `[K`, color codes)
- SSH passphrase prompts block execution

## Investigation Steps

1. Identified that `stdin(Stdio::null())` only controls fd 0 — `/dev/tty` is the controlling terminal, accessible regardless
2. Confirmed `build_child_env()` had no non-interactive defaults set
3. Researched environment variables to suppress interactive behavior:
   - `GIT_TERMINAL_PROMPT=0` — git won't open `/dev/tty`
   - `SSH_ASKPASS_REQUIRE=force` — force SSH to use askpass program
   - `TERM=dumb` — universal signal for no color/tty capabilities
4. Discovered pager variables (`PAGER`, `GIT_PAGER`, etc.) needed separate handling

## Root Cause

The `build_child_env()` function in `server.rs` didn't seed non-interactive defaults. Child processes inherited whatever environment was passed through, with nothing suppressing:
- Terminal prompts via `/dev/tty`
- Interactive pagers
- ANSI color output

## Solution

Added a **layer 1 (lowest precedence)** in `build_child_env()` that seeds non-interactive safe defaults before all other layers. Uses unconditional `push` (not `upsert`) so every subsequent layer can override.

**Credential/prompt suppression:**
```rust
("GIT_TERMINAL_PROMPT", "0"),
("GIT_ASKPASS", "true"),
("SSH_ASKPASS", "true"),
("SSH_ASKPASS_REQUIRE", "force"),
("DEBIAN_FRONTEND", "noninteractive"),
```

**Pager suppression:**
```rust
("PAGER", "cat"),
("GIT_PAGER", "cat"),
("MANPAGER", "cat"),
("SYSTEMD_PAGER", "cat"),
("GH_PAGER", "cat"),
```

**ANSI color suppression:**
```rust
("TERM", "dumb"),
("NO_COLOR", "1"),
("CLICOLOR", "0"),
("FORCE_COLOR", "0"),
```

### Key Design Decision

Each key is seeded as: **host-environment value when set, otherwise the fallback**. This means if the user already has e.g. `PAGER=bat` or `NO_COLOR=0` in their shell, that value is used. Higher-precedence layers (`.env.bash`, `extra_env_passthrough`, `env_overrides`) can override further.

Note that even if the host `TERM` replaces `TERM=dumb`, color output may still be disabled because `NO_COLOR`, `CLICOLOR`, and `FORCE_COLOR` are seeded independently. To fully restore color, override all of them: e.g. `NO_COLOR=0`, `CLICOLOR=1`, `FORCE_COLOR=1` via `env_overrides` or `.env.bash`.

## Why This Works

1. **Host env beats fallbacks**: Each default is seeded as `std::env::var(key).unwrap_or(fallback)`, so the host environment is authoritative. Subsequent layers (allowlist, XDG, .env.bash, extra_env_passthrough, env_overrides) can override further via upsert.

2. **`/dev/tty` bypass prevented**: `GIT_TERMINAL_PROMPT=0` makes git fail cleanly instead of prompting. `SSH_ASKPASS=true` with `SSH_ASKPASS_REQUIRE=force` does the same for SSH.

3. **Pagers neutralized**: `PAGER=cat` makes all pager-capable tools output directly without pausing for user input.

4. **ANSI suppression**: `TERM=dumb` signals to tools that the terminal has no capabilities. Most tools disable color and line-editing. `NO_COLOR=1` provides a fallback for tools that don't check `$TERM`.

## Prevention Strategies

**Test Cases:**
- Assert all defaults present in child env
- Verify `.env.bash` overrides defaults
- Verify `env_overrides` overrides defaults
- Verify allowlist passthrough overrides `TERM` default

**Best Practices:**
- When spawning child processes non-interactively, always seed safe defaults
- Use lowest-precedence defaults so user config can override
- Group defaults by concern (credentials, pagers, color) for maintainability

**Code Review Checklist:**
- [ ] Are interactive prompts suppressed?
- [ ] Are pagers disabled?
- [ ] Is color output suppressed by default?
- [ ] Can users override defaults via their config?

## Related Issues

- **GitHub Issue:** [#374 — Bash command can write to terminal via /dev/tty](https://github.com/dobesv/harnx/issues/374)
- **Plan:** fix-tty-writes-374
- **Related Solution:** [security-issues/environment-sanitization-bash-sandbox-2026-04-29.md](../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md) — Environment variable sanitization for sandboxed processes
