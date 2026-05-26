---
title: "Shebang interpreter dispatch in bash MCP exec/spawn tools"
date: 2026-05-25
category: "logic-errors"
problem_type: logic_error
component: "harnx-mcp-bash"
root_cause: "missing parser for shebang interpreter selection"
resolution_type: code_fix
severity: medium
tags:
  - shebang
  - interpreter
  - sandbox
  - command-execution
  - env-flags
plan_ref: "shebang-support-bash-mcp"
---

## Problem

The `harnx-mcp-bash` exec and spawn tools previously executed all commands via `bash -c`, even when the command string began with a shebang (`#!`) line indicating a different interpreter. Scripts with `#!/usr/bin/env python3` would fail because the Python shebang was ignored and bash attempted to interpret Python code.

## Symptoms

```text
- Behavior: Python/Node/Ruby scripts with shebangs fail with bash syntax errors
- Error example: `syntax error near unexpected token 'print'` for `#!/usr/bin/env python3\nprint("hi")`
- Impact: Feature request #655 — users could not pass multi-line scripts in different languages
```

## Investigation Steps

Reviewed existing exec flow in `exec_command_impl` and `spawn_impl`. Both paths constructed `CommandWrap::with_new("bash", ...)` unconditionally. The `params.command` string was passed directly to `bash -c` without inspection.

Traced the sandbox argument construction sequence: `sb_args.push("--")` separates sandbox options from the command, so interpreter and arguments must be appended after this separator.

Noted that `exec_dir` (per-execution temp directory created via `next_exec_dir()`) already has sandbox write access, making it a suitable location for temp script files.

## Root Cause

Command execution path lacked shebang detection and interpreter dispatch logic. The `command` parameter was treated as opaque bash input regardless of `#!` prefix.

Additionally, for `#!/usr/bin/env -S INTERP` forms, the parser initially treated `-S` as the interpreter name, causing execution failures for valid shebangs using the `-S` split-args flag.

## Solution

### 1. Shebang Parsing

Added `parse_shebang(command)` function in `server.rs`:

```rust
fn parse_shebang(command: &str) -> Option<(PathBuf, Vec<String>)> {
    let first_line = command.lines().next()?;
    let shebang_rest = first_line.strip_prefix("#!")?;
    let mut parts = shebang_rest.split_whitespace();
    let interpreter = parts.next()?;

    if interpreter == "/usr/bin/env" {
        // Skip env flags (e.g. -S) before interpreter name
        let env_interp = parts.find(|t| !t.starts_with('-'))?;
        let extra_args: Vec<String> = parts.map(str::to_string).collect();
        Some((PathBuf::from(env_interp), extra_args))
    } else {
        // Direct path interpreter
        Some((PathBuf::from(interpreter), parts.map(str::to_string).collect()))
    }
}
```

Key pattern: `parts.find(|t| !t.starts_with('-'))` skips env flags like `-S` to find the actual interpreter name.

### 2. Script File Creation

For shebang commands, write to `exec_dir/script.<ext>` and set executable permissions:

```rust
if let Some((interp, shebang_args)) = parse_shebang(&params.command) {
    let ext = shebang_script_ext(&params.command);
    let script_path = exec_dir.join(format!("script.{ext}"));
    std::fs::write(&script_path, params.command.as_bytes())?;
    #[cfg(unix)]
    std::fs::set_permissions(&script_path, Permissions::from_mode(0o755))?;
    // ... execute via interpreter
}
```

File extension mapping: `python`/`python3` → `py`, `node`/`nodejs`/`bun` → `js`, `ruby` → `rb`, etc.

### 3. Sandbox Interpreter Allowlisting

For sandboxed execution with absolute interpreter paths outside `SYSTEM_EXEC_PATHS`:

```rust
if interp.is_absolute() {
    if let Some(interp_dir) = interp.parent() {
        if !Self::SYSTEM_EXEC_PATHS.iter().any(|p| *p == dir_str.as_ref()) {
            sb_args.push(OsString::from("--exec"));
            sb_args.push(interp_dir.as_os_str().to_owned());
        }
    }
}
sb_args.push(interp.as_os_str().to_owned());
for arg in shebang_args {
    sb_args.push(OsString::from(arg));
}
sb_args.push(script_path.as_os_str().to_owned());
```

**Important:** For `#!/usr/bin/env INTERP` (bare name), no `--exec` needed — env resolves via PATH, and sandbox already allows system exec paths.

For absolute paths like `#!/opt/custom/bin/python3`, the parent directory is added to sandbox `--exec` allowlist.

### 4. Cross-Crate Template Filters

MiniJinja filters registered in `harnx-core/src/tool.rs`:

- `shebang_lang`: Returns fence language for Markdown rendering (`python`, `javascript`, etc.)
- `strip_shebang`: Removes shebang line from displayed command

```rust
env.add_filter("shebang_lang", |value: &str| shebang_fence_lang(value).to_string());
env.add_filter("strip_shebang", |value: &str| strip_shebang_line(value).to_string());
```

Both crates share identical env-flag handling: `parts.find(|t| !t.starts_with('-'))`.

## Why This Works

1. **Argv-based execution:** Interpreter, shebang args, and script path passed as discrete `argv` entries — no shell interpolation, no command injection risk.

2. **Sandbox-compatible:** Script lives in `exec_dir` (already writable). Absolute interpreter paths get parent-dir allowlisted via `--exec`. Env-style interpreters rely on `SYSTEM_EXEC_PATHS`.

3. **No extra write args:** Since `exec_dir` is per-execution and sandbox-writable, temp script creation needs no additional `--write` flags.

4. **Unified pattern:** Same logic for exec/spawn and sandbox/non-sandbox — four branches share identical shebang handling sequence.

## Prevention Strategies

**Test Coverage:**
- `test_exec_python_shebang` — non-sandbox Python execution
- `test_spawn_python_shebang` — spawn path with shebang
- `test_sandbox_exec_python_shebang` — Linux sandbox integration
- `test_parse_shebang_env_s_flag` — verifies `-S` flag skipping
- `test_shebang_script_ext_values` — extension mapping verification

**Code Review Checklist:**
- [ ] Shebang detection happens before command execution
- [ ] Script file written to `exec_dir` (sandbox-safe location)
- [ ] Absolute interpreter paths outside system dirs get `--exec` added
- [ ] Env-style interpreters don't get `--exec` (rely on PATH/system exec paths)
- [ ] Env flags like `-S` skipped before interpreter extraction
- [ ] Tests cover sandbox and non-sandbox paths on Linux

## Related Issues

- **GitHub Issue:** #655 — Original feature request
- **Commits:**
  - `0f9497c0` — Initial shebang support implementation
  - `2a00c713` — Fix for `env -S` flag parsing + sandbox test
