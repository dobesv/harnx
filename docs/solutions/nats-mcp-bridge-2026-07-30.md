---
title: "Generic MCP→NATS Bridge (S1)"
date: 2026-07-30
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp-bridge"
root_cause: "stdio MCP servers could not run over NATS without a native per-server rewrite"
resolution_type: code_fix
severity: medium
tags:
  - nats
  - tool-servers
  - mcp
  - bridge
plan_ref: "nats-mcp-bridge-s1"
last_updated: 2026-07-30
---

# Solution: Generic MCP→NATS Bridge

## Problem

stdio MCP servers could not join the NATS tool-server path without being rewritten as native `Toolset` implementations. That approach also could not cover third-party MCP servers.

## Solution

`harnx-mcp-bridge` spawns a wrapped stdio MCP server as an rmcp-client child (`AsyncRwTransport::<RoleClient, _, _>` with `serve_client`, 30s init / 10s list_tools timeouts), caches its tool list, implements the `Toolset` trait, and serves over NATS.

The bridge registers each tool as `{server}_{tool}`, such as `plans_add_note`. On invocation, it validates and strips the `{server}_` prefix before calling the child tool. Result mapping uses bare `serde_json::to_value(CallToolResult)` — byte-identical to the direct MCP path in `harnx-mcp/src/client.rs:651` — so existing `extract_image_parts` and text handling stay compatible.

## Child Lifecycle

The child uses `kill_on_drop(true)`, runs as a `ProcessGroup::leader()`, and sets Linux `PR_SET_PDEATHSIG` (via `pre_exec`) to prevent orphaned processes. A watcher task awaits the `RunningService`'s `waiting()` future and fires a `child_died` `CancellationToken` when the transport closes. The bridge's main loop exits on child death, and the worker supervisor reports its existing soft warning instead of leaving a registered but unusable tool server.

## Error Mapping

Caller-input errors return `ToolInvokeError::Recoverable` so the LLM can self-correct:

- Bad tool-name prefix: `Recoverable("bad tool name {tool}")`
- Non-object / non-null args: `Recoverable("MCP tool arguments must be a JSON object or null")`
- Cancellation: `Recoverable("call cancelled")`

Genuine transport death (`ServiceError::TransportClosed` / `TransportSend(_)`) returns `ToolInvokeError::Fatal` since retry won't help.

## Pitfall: Wrapper Binaries Cannot Use Argv-Sniffing Mode Selection

`run_toolset_main` (harnx-toolset-server) inspects `std::env::args_os()` for `--mcp-stdio` to decide stdio-vs-NATS mode. For a wrapper binary like the bridge, the wrapped child's argv (everything after `--`, e.g. `-- harnx-mcp-plans --mcp-stdio ...`) is part of the wrapper process's own argv.

However, the initial implementation called `run_toolset_main(bridge)`. When the child's `--mcp-stdio` flag was present in argv, `run_toolset_main` matched it and put the bridge itself into MCP-stdio mode instead of NATS mode — the bridge would hang waiting for MCP client input rather than serving NATS requests.

**Fix**: The bridge's `main.rs` does NOT use `run_toolset_main`. It reads `HARNX_INSTANCE_ID` / `HARNX_NATS_URL` / `HARNX_NATS_TOKEN` directly and calls `harnx_toolset_server::serve_over_nats(bridge, ...)` inside a `tokio::select!` racing `child_died.cancelled()`. This bypasses argv-sniffing entirely.

**Lesson**: Any wrapper/passthrough binary must avoid argv-sniffing mode-selection like `run_toolset_main`'s `--mcp-stdio` check. Select mode explicitly — read env vars or config — not by inspecting a potentially-polluted argv.

**Detection**: This bug was only caught by a BINARY-LEVEL test (`bridge_binary_exits_when_wrapped_child_dies` spawning the compiled binary), not by in-process tests that called `serve_over_nats` directly. In-process unit tests masked the collision because they bypassed arg parsing.

## Verification with Plans

S1 migrates `plans` as the first bridge-backed tool server. Tests:

- **integration test**: starts the bridge around `harnx-mcp-plans`, verifies its `plans_*` NATS registration, completes an invocation round trip
- **binary-level child-death test**: kills the wrapped child, asserts bridge exits non-zero
- **raw rmcp-client test**: drives `harnx-mcp-plans --mcp-stdio` directly, proving the plans binary remains available to any MCP client

## Roadmap

This is S1 of issue #1224 (hybrid bridge-first strategy). S2 migrates more roots-free and third-party server configs. S3 adds a roots-over-NATS protocol, S4 passes roots through the bridge, and S5 adds native fs/bash toolsets and deletes `McpManager`. `McpManager` is unchanged in S1.
