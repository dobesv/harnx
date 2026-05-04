---
title: "XDG Directory Separation for Config, Data, and State"
date: 2026-05-03
category: logic-errors
problem_type: logic_error
component: config-paths
root_cause: "flat directory structure mixing user-authored config with runtime data"
resolution_type: code_fix
severity: medium
tags:
  - xdg
  - config
  - data-directory
  - cross-platform
  - test-isolation
  - env-variables
plan_ref: "harnx-store-data-separate-from-config"
---

## Problem

harnx stored all files in a single `~/.config/harnx` directory, mixing user-authored configuration (`.md` instruction files, `config.yaml`) with mutable runtime data (sessions, messages, logs, RAG manifests). This violated XDG conventions and made backup, version control, and system administration harder.

## Symptoms

- Single directory `~/.config/harnx` contained both user-edited files and generated data
- No clean separation between "what the user authors" vs "what the system produces"
- Log files mixed with configuration, polluting version-controlled config directories
- No XDG compliance on Linux desktops

## Investigation Steps

Analyzed existing directory structure and identified path usage patterns:

1. **Audit**: Found all path helpers in `crates/harnx-core/src/config_paths.rs` resolved through `local_path()` which always returned `config_dir().join(name)`

2. **Classification**: Reviewed each path:
   - `config.yaml`, `.env`, `agents/*.md` — user-authored → **config**
   - `sessions/`, `messages.md` — runtime state → **state**
   - `rags/`, `agents/<name>/sessions/` — runtime data → **data**
   - `harnx.log` — log file → **state**

3. **Platform research**: Found `dirs::state_dir()` returns `None` on macOS/Windows — must fall back to `dirs::data_dir()` for cross-platform support

4. **Edge case discovery**: Found `env::var("VAR")` returns `Ok("")` when set to empty string. `PathBuf::from("")` silently writes to CWD

5. **Test isolation gap**: Unit tests only set `HARNX_CONFIG_DIR`, so after split, data/state operations would leak to real user directories

## Root Cause

The original design used a flat directory structure with all paths resolved via `config_dir()`. This baked in the assumption that "harnx has one home directory" rather than separating concerns per XDG spec:
- **Config** (`~/.config/harnx`) — user-authored, may be version-controlled
- **Data** (`~/.local/share/harnx`) — runtime data that persists across restarts
- **State** (`~/.local/state/harnx`) — transient state, logs, caches

When agents were introduced, the `agents/` directory needed to exist in BOTH config (for `.md` instruction files) AND data (for runtime sessions/rags). Several callers were missed in the first pass, causing bugs where code looked for `.md` files in the data directory.

## Solution

### 1. Added `data_dir()` and `state_dir()` functions

```rust
pub fn data_dir() -> PathBuf {
    // 1. HARNX_DATA_DIR override
    // 2. XDG_DATA_HOME/harnx
    // 3. dirs::data_dir()/harnx
}

pub fn state_dir() -> PathBuf {
    // 1. HARNX_STATE_DIR override
    // 2. XDG_STATE_HOME/harnx
    // 3. dirs::state_dir().unwrap_or_else(|| dirs::data_dir())/harnx
}
```

Key: `state_dir()` falls back to `data_dir()` on macOS/Windows where `dirs::state_dir()` returns `None`.

### 2. Split `agents/` directory

Added two separate functions:
- `agents_config_dir()` → `config_dir()/agents/` (for `.md` instruction files)
- `agents_data_dir()` → `data_dir()/agents/` (for runtime data: sessions, rags)

Updated callers:
- `list_agents()` — looks for `.md` files, needed `agents_config_dir()`
- `complete_agent_variables()` — looks for `.md` files, needed `agents_config_dir()`
- `Config::delete("agent")` — deletes `.md` files, needed `agents_config_dir()`

### 3. Added empty-string guards to env var checks

```rust
// BEFORE: accepts empty string silently
if let Ok(v) = env::var(get_env_name("config_dir")) {
    PathBuf::from(v)
}

// AFTER: rejects empty string
if let Ok(v) = env::var(get_env_name("config_dir")) {
    if !v.is_empty() {
        return PathBuf::from(v);
    }
}
```

This prevents `PathBuf::from("")` which resolves to CWD.

### 4. Updated test isolation helpers

```rust
fn with_test_config_dir<T>(f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let config_dir = unique_test_config_dir();
    let data_dir = config_dir.with_file_name(format!("{}-data", ...));
    let state_dir = config_dir.with_file_name(format!("{}-state", ...));
    
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", &config_dir);
        std::env::set_var("HARNX_DATA_DIR", &data_dir);
        std::env::set_var("HARNX_STATE_DIR", &state_dir);
    }
    // ... cleanup all three on exit
}
```

## Why This Works

1. **XDG compliance**: Linux users get proper `~/.local/share/harnx` and `~/.local/state/harnx` directories, while macOS/Windows fall back gracefully

2. **Separation of concerns**: User-authored config is cleanly separated from runtime data, enabling:
   - Version control of config without runtime artifacts
   - Separate backup policies for config vs data
   - Easier system administration

3. **Safe env handling**: Empty-string guards prevent silent CWD writes when env vars are explicitly set to `""`

4. **Test isolation**: Tests that set only `HARNX_CONFIG_DIR` would previously leak data operations to real user directories; now all three dirs are isolated

## Prevention Strategies

### Test Cases

- Add tests verifying XDG env vars (`XDG_DATA_HOME`, `XDG_STATE_HOME`) are respected
- Add tests for empty-string env var rejection
- Add tests verifying `state_dir()` fallback to `data_dir()` on non-Linux platforms
- Extend `with_test_config_dir` pattern to all test helpers that manipulate paths

### Best Practices

- Always guard env var checks with `!value.is_empty()` before constructing paths
- When introducing new directory categories, immediately update ALL test helpers
- Use `agents_config_dir()` for user-authored files, `agents_data_dir()` for runtime data
- On `dirs` crate API: check platform support — `state_dir()` returns `None` on macOS/Windows

### Code Review Checklist

- [ ] New paths go to the correct XDG category (config/data/state)?
- [ ] Env var checks have `!value.is_empty()` guards?
- [ ] Test helpers set all three directories (config/data/state)?
- [ ] Cross-platform fallback for `dirs::state_dir()`?
- [ ] Agent-related paths distinguish config (`.md` files) vs data (runtime)?

## Related Issues

- **Plan:** `harnx-store-data-separate-from-config`
- **Commits:**
  - `34459f4` feat(config): add data_dir/state_dir and redirect data/state paths
  - `ad5041a` test(config): add unit tests for data_dir/state_dir path resolution
  - `148ec0f` feat(config): redirect harnx.log default to state dir
  - `a7ace76` feat(config): separate agent .md config from agent runtime data
  - `c619a50` fix(config): reject empty-string env overrides and add XDG fallback tests
  - `206cd2c` fix(config): fix delete-agent path and isolate runtime test dirs
