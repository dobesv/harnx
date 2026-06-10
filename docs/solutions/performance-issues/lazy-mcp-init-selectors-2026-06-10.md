---
title: "Lazy MCP server initialization via selector pre-filtering"
date: 2026-06-10
category: performance-issues
problem_type: performance_issue
component: mcp-manager
root_cause: eager connection of all configured MCP servers regardless of agent tool selectors
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - lazy-initialization
  - tool-selectors
  - glob-patterns
  - toolsets
plan_ref: mcp-lazy-init-790
---

## Problem

Harnx connected to every configured MCP server at session start to enumerate tools, even when the active agent's `use_tools` selectors meant none of that server's tools would be exposed. This produced spurious "MCP server failed to connect" warnings for unused servers and paid their startup cost needlessly.

## Symptoms

- Spurious warnings at session start: `MCP server 'context7' failed to connect` for servers the agent never uses
- Unnecessary latency and resource usage connecting to all configured MCP servers
- Confusing user experience: errors for servers that shouldn't matter

## Investigation Steps

1. Traced session startup to `Config::tool_declarations_for_use_tools` in `crates/harnx-runtime/src/config/mod.rs`
2. Found it called `McpManager::get_all_tools_blocking()` which eagerly connects ALL clients
3. `use_tools` selector filtering happened AFTER full discovery, so unused servers connected regardless
4. Recognized that tool display names are deterministically `{server_name}_{tool_name}` unless renamed
5. Hypothesis: can decide which servers to connect BEFORE enumeration by matching selectors against known server prefixes

## Root Cause

`McpManager::get_all_tools` eagerly connected every configured MCP client to list their tools. The `use_tools` filtering in `Config::select_tools` happened after discovery, so all servers started regardless of whether their tools would be used.

Key insight: MCP tool display names follow a predictable pattern (`{server_name}_{tool_name}`) unless overridden by `rename_tools`. This allows conservative pre-filtering: determine which servers an agent might use before connecting, by matching selector patterns against server name prefixes.

## Solution

Added `McpManager::get_tools_for_selectors(selectors)` and blocking variant that only connects clients where `selector_could_match_server` returns true.

### Selector Pre-Filter Rules

The `selector_could_match_server` function implements conservative matching (never false-negative, only harmless false-positives):

1. `*` selector → connect all servers (preserves prior eager behavior)
2. Exact selector (no glob metachars) → true iff it starts with `{server}_`
3. Glob selector → compare its literal leading prefix (up to first metachar) against `{server}_` — connect if either is a prefix of the other
4. Renamed tools → check selector against `rename_tools` display names explicitly (they drop the server prefix)

### Code Pattern

**Before:**
```rust
// In tool_declarations_for_use_tools
declarations.extend(manager.get_all_tools_blocking());
```

**After:**
```rust
// In tool_declarations_for_use_tools
let selectors = split_tool_selectors(use_tools)
    .into_iter()
    .flat_map(|selector| {
        self.toolsets.get(selector).cloned()
            .unwrap_or_else(|| vec![selector.trim().to_string()])
    })
    .collect::<Vec<String>>();

if selectors.iter().any(|selector| selector == "*") {
    declarations.extend(manager.get_all_tools_blocking());
} else {
    declarations.extend(manager.get_tools_for_selectors_blocking(&selectors));
}
```

### Already-Connected Servers

Already-connected servers remain included unconditionally (`client.is_connected() || ...`). This ensures hot-path tool calls don't suffer extra round-trips. Side effect: tools from pre-connected servers may appear in eval context even if not matched by current selectors — acceptable trade-off for performance.

## Why This Works

Conservative pre-filtering never hides a real tool. A false positive only costs one extra connection (harmless). A false negative would break tool availability (unacceptable).

Glob pattern matching depends on literal prefix comparison:
- `fs_*` → literal prefix `fs_` → matches server `fs`
- `f*` → literal prefix `f` → matches server `fs` (prefix of `fs_`)
- `*_read` → empty literal prefix → matches all servers conservatively
- `{fs_read,bash_exec}` → starts with `{` → matches all servers conservatively

Renamed tools handled explicitly because they drop the server prefix entirely.

## Prevention Strategies

### Test Coverage

- Unit tests for `selector_could_match_server` (9 tests covering: `*`, exact match/mismatch, server glob match/mismatch, partial prefix, leading metachar, renamed tool match/mismatch)
- Integration tests for `get_tools_for_selectors` (exact filter, glob, `*` includes all, renamed-tool server)
- Stress testing (`cargo nextest run --workspace --stress-count=5`) caught environment race

### Environment Race Fix

Test flake surfaced by timing change: `test_init_mcp_manager_with_roots` read `HOME` env without holding `env_lock()`, racing other tests using `HomeGuard`. Fix: acquire `env_lock()` with `#[cfg(unix)]` guard.

### Best Practices

- Always use `env_lock()` when reading/writing environment variables in tests
- Conservative pre-filters must never false-negative for correctness
- Test blocking discovery lifecycle across all runtime flavors (MultiThread, CurrentThread, no runtime)

## Pitfalls / Lessons

1. **Toolset expansion must happen before filtering.** The handoff-check gate must use expanded selectors, not raw `use_tools`, or toolsets containing `*` or `*_session_handoff` won't generate handoff declarations.

2. **`*` selector preserves eager fallback.** Both `get_tools_for_selectors` and runtime integration check for `*` first and delegate to `get_all_tools()`, preserving backward compatibility.

3. **`matches_tool_glob` duplication.** Duplicated in `harnx-mcp` to preserve dependency direction (harnx-runtime depends on harnx-mcp, not vice versa). Could consolidate if needed, but local copy avoids circular dependency.

4. **MultiThread blocking discovery intentionally skips `invalidate_all_services()`.** The caller's runtime persists, so keeping connections avoids reconnect churn. CurrentThread/no-runtime branches use short-lived runtimes and must invalidate.

5. **Leading-metachar globs match conservatively.** Selectors like `*_read` or `{a,b}` have empty literal prefixes and match every server (safe over-connection).

## Related Issues

- **GitHub Issue:** [#790](https://github.com/dobesv/harnx/issues/790) — MCP servers initialized even when agent doesn't use their tools
