---
title: "Package-relative agent delegation resolution for _session_*"
date: 2026-06-09
category: logic-errors
problem_type: logic_error
component: harnx-runtime
root_cause: qualified-server-name-in-acp-display-name
resolution_type: code_fix
severity: medium
tags:
  - session-delegation
  - packages
  - namespacing
  - agent-resolution
plan_ref: issue-709-package-delegation
---

## Problem

Agent delegation tools (e.g., `_session_prompt`, `_session_new`) did not resolve package-relative targets when called from within a package. Furthermore, the fully-qualified agent names (e.g., `pkg/agent`) leaked into the LLM function names, violating function-naming schemas (which typically forbid `/`) and preventing same-package peers from addressing each other by bare name.

## Symptoms

- A package agent `pantheon/daedalus` saw delegation tools like `pantheon/atlas_session_prompt` instead of the expected `atlas_session_prompt`.
- LLM calls failed due to invalid function names containing slashes.
- Delegation failed to resolve targets correctly because the display names did not match the expected relative-naming convention.

## Root Cause

Auto-registered package agents populate `AcpServerConfig.name` with a fully-qualified name (`pkg/agent`). However, `acp_server_display_name` in `crates/harnx-runtime/src/config/patches_split.rs` assumed `server.name` was always a bare stem. This resulted in slashes leaking into the display names used for tool generation and dispatch.

## Fix

The fix establishes `acp_server_display_name` as the lean, single source of truth for delegation tool naming. It now reconstructs the target's qualified name based on the server configuration and delegates formatting to the shared `harnx_core::package_namespace::handoff_display_name` helper.

Because delegation tool generation (`harnx-acp::generate_acp_tools`) and dispatch (`find_client_for_tool`) both key off the `AcpManager.clients` display-name keys, fixing this single function ensures consistency across the entire delegation flow without requiring an additional engine-level mapping.

### Why MCP was left unchanged

The `mcp_server_display_name` function was deliberately left with its original logic. MCP tool resolution uses a different mechanism (`namespace_use_tools_entry`), where top-level MCP servers (e.g., `bash`) are expected to remain bare rather than using the `__` prefix used by the agent handoff/delegation scheme. Reusing the handoff scheme for MCP would have regressed existing contracts (e.g., `package_loading_test_mcp_server_display_names_for_agent`).

## Resolution Table

From the perspective of an active package `P`, delegation tool prefixes now match the handoff scheme:

| Target | Display tool prefix | Example tool |
|---|---|---|
| Same-package peer `P/atlas` | `atlas` | `atlas_session_prompt` |
| Cross-package peer `other/helper` | `other__helper` | `other__helper_session_prompt` |
| Top-level agent `global` (in pkg) | `__global` | `__global_session_prompt` |
| (Active top-level) package peer `P/atlas` | `P__atlas` | `P__atlas_session_prompt` |
| (Active top-level) top-level peer `global` | `global` | `global_session_prompt` |

No tool names contain `/`.

## Key Gotchas

1. **`strip_suffix` vs `trim_end_matches`**: Always use `strip_suffix` for exact single-pattern removal. `trim_end_matches` is greedy and may over-trim agent names that happen to end with the pattern.
2. **Ambiguous Decoding**: Package and agent names may contain underscores (`_`), so decoding by splitting on `__` is unreliable. This implementation avoids the need for decoding by ensuring that both tool generation and dispatch use the same computed key.
3. **Dispatch Logic**: The dispatch prefix-match logic remains unchanged; it simply benefits from the corrected keys in the client map.

## Tests Added

- **Unit tests for name resolution**: Added 8 tests to `patches_split.rs` covering 4 ACP scenarios (same-pkg, cross-pkg, top-level-from-pkg, top-level-context) and 4 MCP scenarios.
- **Round-trip verification**: Added tests in `harnx-acp/src/manager.rs` to ensure that generated tool names correctly dispatch back to the intended client keys, covering namespaced (`other__helper`), top-level (`__global`), and underscore-containing stems (`my_agent`).

## Related Issues

- **GitHub Issue:** [#709](https://github.com/dobesv/harnx/issues/709) — Agent handoff from package agent goes to non-package agent (delegation follow-up)
- **Sibling Resolution:** [Package-relative agent handoff resolution for _session_handoff](./package-relative-agent-handoff-2026-06-04.md)
