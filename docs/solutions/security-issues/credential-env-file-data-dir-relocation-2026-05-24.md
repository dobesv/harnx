---
title: "Credential Env Files Relocated to XDG Data Directory"
date: 2026-05-24
category: security-issues
problem_type: security_issue
component: config-paths
root_cause: "sensitive credential files stored in XDG config dir accessible to MCP servers"
resolution_type: code_fix
severity: medium
tags:
  - credentials
  - xdg
  - security
  - permissions
  - env-files
plan_ref: "relocate-credential-env-files"
---

## Problem

Credential env files (`.env`, `.env.bash`) were stored in the XDG config directory (`~/.config/harnx/`), making API keys accessible to agents with config-dir MCP server access. Agents could read `.env` files through config directory file access.

## Symptoms

- API keys stored in `~/.config/harnx/.env` and `~/.config/harnx/.env.bash`
- Agents with MCP servers that have config directory access could read credential files
- No permission enforcement on credential file access
- Bash MCP server had duplicated XDG resolution logic for `.env.bash` path

## Investigation Steps

1. Reviewed prior art in `docs/solutions/logic-errors/xdg-directory-separation-2026-05-03.md` — established XDG data dir pattern for runtime data
2. Identified credential files as security-sensitive data, not user-authored config
3. Checked existing permission enforcement patterns in harnx-runtime — found silent `let _ =` pattern at line 3666
4. Traced bash MCP server's `.env.bash` loading — found inline `bash_config_dir()` resolution logic
5. Injected `KEY=value` parsing in `load_env_file()` — discovered panic on malformed `=VALUE` lines with empty key

## Root Cause

1. **Wrong directory**: Credential files were placed in `~/.config/harnx/` (XDG config dir) alongside user-authored config files. MCP servers with config-dir access could read these files.

2. **Duplicated logic**: Bash MCP server had its own `bash_config_dir()` function for `.env.bash` resolution instead of using centralized `harnx-core::config_paths`.

3. **No permission enforcement**: Credential files had no explicit `0600` permission enforcement, relying on user's umask.

4. **Panic on malformed .env**: `load_env_file()` passed empty key to `env::set_var()` on lines like `=VALUE`, causing panic.

## Solution

### 1. Relocated credential files to XDG data directory

Changed `env_file()` in `harnx-core/src/config_paths.rs`:

```rust
// BEFORE
pub fn env_file() -> PathBuf {
    match env::var(get_env_name("env_file")) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => local_path(ENV_FILE_NAME),  // ~/.config/harnx/.env
    }
}

// AFTER
pub fn env_file() -> PathBuf {
    match env::var(get_env_name("env_file")) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => data_path(ENV_FILE_NAME),  // ~/.local/share/harnx/.env
    }
}
```

### 2. Added `bash_env_file()` with HARNX_BASH_ENV_FILE override

Added centralized path resolution in `harnx-core/src/config_paths.rs`:

```rust
/// Bash env file loaded by the bash MCP server.
pub const BASH_ENV_FILE_NAME: &str = ".env.bash";

/// Path to the `.env.bash` file loaded by the bash MCP server. Overridable via `HARNX_BASH_ENV_FILE`.
pub fn bash_env_file() -> PathBuf {
    match env::var(get_env_name("bash_env_file")) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => data_path(BASH_ENV_FILE_NAME),
    }
}
```

Bash MCP server now imports from harnx-core:

```rust
// BEFORE: inline bash_config_dir() function
let env_file = bash_config_dir().join(".env.bash");

// AFTER: centralized resolution
let env_file = harnx_core::config_paths::bash_env_file();
```

### 3. Added 0600 permission enforcement

Enforced restrictive permissions after successful file read, using existing project patterns:

```rust
let env_file = harnx_core::config_paths::bash_env_file();
let Ok(contents) = std::fs::read_to_string(&env_file) else {
    return vec![];
};
#[cfg(unix)]
{
    use std::os::unix::prelude::PermissionsExt;
    let _ = std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600));
}
```

Same pattern applied in `harnx-runtime/src/config/mod.rs:load_env_file()`.

### 4. Fixed empty-key panic in .env parsing

Added guard before `env::set_var()`:

```rust
// BEFORE
if let Some((key, value)) = line.split_once('=') {
    unsafe {
        env::set_var(key.trim(), value.trim());
    }
}

// AFTER
if let Some((key, value)) = line.split_once('=') {
    let key = key.trim();
    if !key.is_empty() {
        unsafe {
            env::set_var(key, value.trim());
        }
    }
}
```

## Why This Works

1. **XDG separation**: Data directory (`~/.local/share/harnx/`) is for runtime data. Config directory (`~/.config/harnx/`) is for user-authored config. MCP servers with config-dir access no longer see credential files.

2. **Centralized path resolution**: `bash_env_file()` in harnx-core eliminates duplicated XDG logic. Both `.env` and `.env.bash` follow same pattern with env var overrides (`HARNX_ENV_FILE`, `HARNX_BASH_ENV_FILE`).

3. **Permission enforcement**: Silent `let _ =` matches existing project pattern. `#[cfg(unix)]` ensures Windows compatibility. Applies after successful read to avoid errors on missing files.

4. **Empty-string guards**: Prior art from `xdg-directory-separation` established `!value.is_empty()` pattern for env var overrides. Applied same pattern to `HARNX_BASH_ENV_FILE`.

## Prevention Strategies

### Test Cases

- `env_file_default_returns_data_dir_env` — verifies `.env` defaults to data dir
- `bash_env_file_default_returns_data_dir_env_bash` — verifies `.env.bash` defaults to data dir
- `bash_env_file_empty_falls_back_to_default` — verifies empty `HARNX_BASH_ENV_FILE` falls back to default
- `harnx-mcp-bash::env_bash_dotfile_loaded` — verifies `.env.bash` loading works end-to-end

### Best Practices

- Store credentials in XDG data directory, not config directory
- Always guard env var checks with `!value.is_empty()` before constructing paths
- Use `#[cfg(unix)]` for permission enforcement to maintain Windows compatibility
- Apply permissions after successful file read with silent `let _ =` error handling
- Centralize path resolution in harnx-core to avoid duplication

### Code Review Checklist

- [ ] Are credential files in data directory, not config directory?
- [ ] Does `load_*_env_file()` have `#[cfg(unix)]` permission enforcement?
- [ ] Are env var overrides guarded with `!value.is_empty()`?
- [ ] Is path resolution imported from `harnx_core::config_paths` rather than duplicated?
- [ ] Do .env parsing loops handle empty keys gracefully?

## Related Issues

- **Prior Art:** [logic-errors/xdg-directory-separation-2026-05-03.md](../logic-errors/xdg-directory-separation-2026-05-03.md) — established XDG data dir pattern and empty-string guard pattern
- **Issue:** #637 — original issue tracking credential file relocation
- **Decision Note:** Plan note `4c50d3fa` — key decisions including no auto-migration, silent permission enforcement pattern
