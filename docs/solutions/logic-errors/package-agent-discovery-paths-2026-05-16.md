---
title: "Package agent discovery for picker and variable completion"
date: 2026-05-16
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime, harnx-config"
root_cause: "list_assistant_agents and complete_agent_variables only scanned top-level agents/ directory"
resolution_type: code_fix
severity: medium
tags:
  - package-agents
  - agent-picker
  - path-resolution
  - completions
plan_ref: "fix-agent-picker-packages-569"
---

## Problem

Agents defined in `packages/<pkg>/agents/*.md` were invisible to the TUI agent picker and `.agent` tab completion. The functions `list_assistant_agents()` and `complete_agent_variables()` only scanned the top-level `agents/` directory, ignoring package-resident agents entirely.

Related: variable completion for `.agent pkg/name VAR=` failed because `complete_agent_variables()` constructed paths incorrectly for qualified names.

## Symptoms

- Package agents (e.g., `mypkg/coder`) did not appear in TUI picker
- Tab completion for `.agent` command excluded package agents
- Variable completion (`.agent pkg/name VAR=`) returned no results for package agents
- Workaround: direct invocation with `harnx -a pkg/name` worked correctly

## Root Cause

Two separate bugs:

### 1. `list_assistant_agents()` incomplete scan

Function only iterated `Config::agents_config_dir()` (top-level `agents/`), missing the package-scanning logic that `list_agents()` already had.

### 2. `complete_agent_variables()` wrong path construction

Function used manual string concatenation:

```rust
// Wrong: assumes flat agents/ directory
let markdown_path = Config::agents_config_dir().join(format!("{agent_name}.md"));
```

For `agent_name = "pkg/name"`, this produced `agents/pkg/name.md` instead of `packages/pkg/agents/name.md`.

## Solution

### Fix 1: Mirror `list_agents()` package scanning in `list_assistant_agents()`

Added package directory iteration after top-level scan (lines 475-509):

```rust
// Also include assistant agents from packages with qualified names (pkg/stem)
let packages_dir = harnx_core::config_paths::packages_dir();
if let Ok(pkg_entries) = read_dir(&packages_dir) {
    for pkg_entry in pkg_entries.filter_map(|e| e.ok()) {
        let pkg_path = pkg_entry.path();
        if !pkg_path.is_dir() {
            continue;
        }
        let pkg_name = match pkg_path.file_name().and_then(|n| n.to_str()) {
            Some(n) if !n.starts_with('.') => n.to_string(),
            _ => continue,
        };
        let agents_dir = pkg_path.join(harnx_core::config_paths::AGENTS_DIR_NAME);
        if let Ok(agent_entries) = read_dir(&agents_dir) {
            for agent_entry in agent_entries.filter_map(|e| e.ok()) {
                let path = agent_entry.path();
                if path.extension().and_then(|x| x.to_str()) != Some("md") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(contents) = read_to_string(&path) else {
                    continue;
                };
                let qualified = format!("{pkg_name}/{stem}");
                if let Ok(config) = AgentConfig::from_markdown(&qualified, &contents) {
                    if config.role == AgentRole::Assistant {
                        output.push(qualified);
                    }
                }
            }
        }
    }
}

output.sort();
output.dedup();
```

### Fix 2: Use `Config::agent_file()` for path resolution

Changed from manual path construction to centralized resolver:

```rust
pub fn complete_agent_variables(agent_name: &str) -> Vec<(String, Option<String>)> {
    let markdown_path = Config::agent_file(agent_name);  // ← correct
    if markdown_path.exists() {
        // ...
    }
}
```

`Config::agent_file()` correctly handles both cases:

```rust
pub fn agent_file(name: &str) -> PathBuf {
    if let Some((pkg, stem)) = name.split_once('/') {
        paths::package_dir(pkg)
            .join(paths::AGENTS_DIR_NAME)
            .join(format!("{stem}.md"))
    } else {
        paths::agents_config_dir().join(format!("{name}.md"))
    }
}
```

## Why This Works

- **Package scanning**: Mirrors the existing pattern in `list_agents()` — iterates `packages_dir()`, skips hidden directories, finds `agents/` subdir, filters `.md` files, formats as `pkg/stem`
- **Role filtering**: Parses frontmatter to check `config.role == AgentRole::Assistant` before including
- **Graceful degradation**: Uses `if let Ok(...)` and `continue` to skip unreadable files/directories without failing the entire operation
- **Centralized path resolution**: `Config::agent_file()` is the single source of truth for agent path resolution — any code that needs to open an agent file by name should use it

## Prevention Strategies

**Test isolation pattern (`ENV_MUTEX` + `EnvGuard`):**

```rust
use std::sync::LazyLock, Mutex;

static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[test]
fn test_package_agent_discovery() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());
    // ... test code ...
}
```

**Test helper pattern (`install_test_package`):**

```rust
fn install_test_package(config_dir: &Path, pkg_name: &str, files: &[(&str, &str)]) {
    let pkg_dir = config_dir.join("packages").join(pkg_name);
    fs::create_dir_all(&pkg_dir).unwrap();
    
    let manifest = "name: ".to_string() + pkg_name;
    fs::write(pkg_dir.join("manifest.yaml"), manifest).unwrap();
    
    for (path, content) in files {
        fs::write(pkg_dir.join(path), content).unwrap();
    }
}
```

**Code review checklist:**

- [ ] Any code that resolves agent file paths uses `Config::agent_file(name)`, not manual string concatenation
- [ ] Functions that iterate agents check both top-level `agents/` and `packages/*/agents/`
- [ ] New path logic tests use `ENV_MUTEX` to prevent parallel test interference
- [ ] Qualified names (`pkg/stem`) handled consistently across discovery and resolution

**Test coverage:**

- `package_loading_test_package_assistant_agent_appears_in_list_assistant_agents` — package assistant appears in list
- `package_loading_test_package_subagent_not_in_list_assistant_agents` — role filtering works
- `package_loading_test_multiple_packages_assistant_sorted_deduped` — multi-package aggregation
- `package_loading_test_package_agent_variable_completion` — `complete_agent_variables` resolves qualified names

## Related Issues

- **Issue:** [GH-569](https://github.com/SmartestEdu/harnx/issues/569) — Agent picker does not show agents from packages
- **Related Solution:** [logic-errors/agent-role-filtering-completions-2026-05-04.md](agent-role-filtering-completions-2026-05-04.md) — AgentRole enum and list_assistant_agents() original implementation
- **Related Solution:** [integration-issues/package-system-implementation-patterns-2026-05-07.md](../integration-issues/package-system-implementation-patterns-2026-05-07.md) — Package system patterns

## Addition: harnx-pkg binary distribution

As part of this fix, the `harnx-pkg` binary was added to distribution channels. Pattern for adding a new binary:

1. **Argcfile.sh `install()`**: Add to `@arg bins` allowed values and default array
2. **docker/harnx.Dockerfile**: Add `COPY --from=builder` line
3. **.github/workflows/release.yaml**: Add to:
   - Build matrix (`-p harnx-pkg`)
   - `archive_specs` for each platform
   - Docker download patterns (both architectures)
   - Verify step
