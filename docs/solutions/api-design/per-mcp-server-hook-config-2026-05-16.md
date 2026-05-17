---
title: "Per-MCP-server hook configuration"
date: 2026-05-16
category: "api-design"
problem_type: integration_issue
component: "harnx-runtime"
root_cause: "missing scope isolation for hook definitions"
resolution_type: code_fix
severity: medium
tags:
  - hooks
  - mcp
  - scoping
  - configuration
plan_ref: "per-mcp-server-hooks"
---

## Problem

Hook definitions lived only in the global `config.yaml`, making it impossible to scope hooks to a specific MCP server. Users could not define hooks inside a per-server YAML file that applied only to tool calls targeting that server.

## Symptoms

- Hooks in global config applied to all tool calls regardless of origin
- No mechanism for package authors to ship server-scoped hooks
- Renaming MCP servers (prefixing for packages) would require updating all hook matchers

## Investigation Steps

1. Traced the hook dispatch flow: `build_tool_eval_context` builds the `DispatchHookFn` closure over `hooks.entries`
2. Identified that `McpManager` stores clients keyed by **display name** (e.g., `pkg__bash`), not original config name
3. Found that `ToolDeclaration.mcp_server_name` is set to `self.name` in `McpClient::list_tools` — also the display name
4. Realized direct lookup via `Config.mcp_servers` (which has original names) would fail silently for packaged servers

## Root Cause

Hook configuration was monolithic. The dispatch closure had no visibility into which MCP server a tool call originated from, and there was no mechanism to scope hook definitions to a per-server context.

## Solution

Extended `McpServerConfig` with an optional `hooks` field and implemented server-scoped hook dispatch:

**1. Added hooks field to McpServerConfig (`crates/harnx-mcp/src/config.rs`):**

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    // ... other fields ...
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
}
```

**2. Added mcp_server_name to ToolDeclaration (`crates/harnx-core/src/tool.rs`):**

```rust
pub struct ToolDeclaration {
    // ... other fields ...
    #[serde(skip, default)]
    pub mcp_server_name: Option<String>,
}
```

**3. Added matches_str to CompiledMatcher (`crates/harnx-hooks/src/matcher.rs`):**

```rust
impl CompiledMatcher {
    /// Match against an explicit text string instead of extracting from an event.
    /// Used for server-scoped hooks where the matcher applies to the bare (unprefixed)
    /// tool name rather than the display name carried in the event.
    pub fn matches_str(&self, text: &str) -> bool {
        match self {
            CompiledMatcher::Regex(re) => re.is_match(text),
            CompiledMatcher::Glob(pat) => pat.matches(text),
        }
    }
}
```

**4. Build per-tool hooks map and merge at dispatch (`crates/harnx-runtime/src/tool.rs`):**

```rust
// Build map from display tool name → (bare_tool_name, server_hook_entries)
let per_tool_hooks: HashMap<String, (String, Vec<HookConfig>)> = decl_map
    .iter()
    .filter_map(|(tool_name, decl)| {
        let server_name = decl.mcp_server_name.as_ref()?;
        let bare_name = decl.mcp_tool_name.clone()
            .unwrap_or_else(|| tool_name.clone());
        let server_hooks = mcp_manager
            .as_ref()?
            .get_client(server_name)
            .and_then(|client| client.hooks().cloned())?;
        let hook_entries = server_hooks.entries.iter()
            .filter(|hook| matches!(hook.event.as_str(), "PreToolUse" | "PostToolUse" | "PostToolUseFailure"))
            .cloned()
            .collect::<Vec<_>>();
        (!hook_entries.is_empty()).then(|| (tool_name.clone(), (bare_name, hook_entries)))
    })
    .collect();

// In dispatch closure: match vs bare name, strip matcher before merge
let mut merged_entries: Vec<HookConfig> = if let Some(display_name) = display_tool_name {
    per_tool_hooks.get(display_name)
        .map(|(bare_name, entries)| {
            entries.iter()
                .filter(|hook| {
                    harnx_hooks::CompiledMatcher::compile(&hook.matcher)
                        .map(|m| m.matches_str(bare_name))
                        .unwrap_or(false)
                })
                .map(|hook| HookConfig { matcher: None, ..hook.clone() })
                .collect()
        })
        .unwrap_or_default()
} else { Vec::new() };
merged_entries.extend(hooks_entries.clone());
```

## Why This Works

**Server name identity via McpManager**: MCP servers get prefixed names (e.g., `bash` → `pkg__bash`) in `reinit_managers_for_agent`. `McpManager` stores clients by this display name, and `ToolDeclaration.mcp_server_name` matches it. Hook lookup via `mcp_manager.get_client(server_name)` finds packaged servers correctly; direct `Config.mcp_servers` lookup would silently fail.

**Bare-name matcher semantics**: Server hook matchers evaluate against the bare tool name (`exec`) not the display name (`bash_exec`). Hook configs remain portable — renaming the MCP server doesn't require updating matchers. Implementation calls `CompiledMatcher::matches_str(bare_name)` instead of `matcher.matches(event)`.

**Matcher stripping trick**: After filtering, the `matcher` field is cleared before adding to the merged list. This prevents the global dispatcher from re-running the matcher against the prefixed display name (which would fail for matchers like `^exec$` when event's tool_name is `bash_exec`).

**Event type filtering**: Server hooks are filtered to only `PreToolUse`, `PostToolUse`, `PostToolUseFailure` at context build time. Other events (`SessionStart`, `Stop`, etc.) are silently ignored.

**Merge order**: Server-specific hook entries prepend to global entries, ensuring they run first and can block before global hooks see the event.

## Prevention Strategies

**Test Cases:**
- Verify server hooks run before global hooks
- Test that matcher `^exec$` matches `exec` but not `bash_exec` when defined in server config
- Confirm packaged servers (`pkg__server`) hooks dispatch correctly
- Assert non-tool events (`SessionStart`, `Stop`) are ignored in server hooks

**Best Practices:**
- Use bare tool names in server hook matchers for portability
- Filter server hooks to tool-use events only (others ignored anyway)
- Prefer server-scoped hooks for package-provided blocking logic

**Code Review Checklist:**
- [ ] Is server hook lookup going through `McpManager`, not raw `Config.mcp_servers`?
- [ ] Are matchers evaluated against bare names, then stripped before merge?
- [ ] Are non-tool events filtered out of server hook entries?
- [ ] Is merge order correct (server hooks first)?

## Related Issues

- **Files Changed:**
  - `crates/harnx-mcp/src/config.rs` — `hooks: Option<HooksConfig>` on `McpServerConfig`
  - `crates/harnx-mcp/src/client.rs` — set `mcp_server_name` on tool declarations; expose `hooks()` accessor
  - `crates/harnx-core/src/tool.rs` — `mcp_server_name: Option<String>` on `ToolDeclaration` (serde skip)
  - `crates/harnx-hooks/src/matcher.rs` — `matches_str(&self, text: &str) -> bool` on `CompiledMatcher`
  - `crates/harnx-runtime/src/tool.rs` — `build_tool_eval_context` and `build_dispatch_hook_fn` extended
