---
title: "Instance-Scoped NATS Tool Servers — Phase 2a Pilot"
date: 2026-07-29
category: "integration-issues"
problem_type: integration_issue
component: "harnx-runtime/harnx-toolset"
root_cause: "worker-owned tools only supported stdio transports and had no instance-scoped NATS invocation protocol"
resolution_type: code_fix
severity: high
tags:
  - nats
  - tool-servers
  - transport
  - instance-scoping
plan_ref: "nats-tool-servers-phase2a"
---

# Solution: Instance-Scoped NATS Tool Servers

## Problem

Phase 1 routed local front ends through a shared NATS broker and worker, but tools still ran through in-process or stdio providers. Moving every tool at once would have mixed protocol design, process supervision, and many tool migrations in one change.

Phase 2a establishes the transport with one low-risk pilot, `harnx-time-server`. Existing MCP and ACP stdio tools remain available in the same worker turn. Phase 2b will generalize bootstrap configuration, migrate remaining toolsets and sub-agents, and eventually remove stdio.

## Instance identity and subjects

Each worker creates one `InstanceId` at daemon startup and shares it with all tool servers it owns. Its value is `{worker_pid}-{uuid_v4}`. Children receive the same value in `HARNX_INSTANCE_ID`; they don't generate another identity.

Tool calls use Core NATS request-reply:

```text
harnx.v1.{instance_id}.tools.{server}.{tool}
```

Cancellation and future progress messages share one low-cardinality subject per worker instance:

```text
harnx.v1.{instance_id}.tools.control
```

The control message and `X-Harnx-Call-Id` header correlate cancellation to a call. Call IDs don't appear in subject names. Tool invocation and control traffic use Core NATS only. JetStream is limited to the registration KV bucket.

## Request and idempotency contract

Every request carries:

- `Idempotency-Key`: logical invocation identity used by the server's 60-second reply cache.
- `X-Harnx-Call-Id`: in-flight call and cancellation correlation.
- `X-Harnx-Instance-Id`: worker instance validation and diagnostics.
- `Content-Type: application/json`.

Core NATS request-reply is at-most-once transport, not durable delivery. Reusing an idempotency key returns the cached reply instead of executing a side-effecting tool twice. `ToolRequest` and `ToolReply` preserve the call ID in the JSON body, and recoverable versus fatal errors remain explicit on the wire.

## Registration is discovery, not liveness

Servers publish `Registration` JSON to JetStream KV bucket `harnx_tool_registry` under `{instance_id}.{server}`. The record contains tool schemas, read-only/idempotent hints, and protocol/schema versions. The worker creates a history-aware watch before spawn and waits for the pilot registration before publishing worker readiness or accepting a turn.

KV refresh is for discovery and schema publication only. It has no TTL heartbeat and isn't a crash detector. A stale heartbeat has an ambiguity window, while Linux pidfd process monitoring reports the exact owned child exit immediately without PID-reuse races. The supervisor fails matching `NatsInFlightCalls` as recoverable, then deletes the registration. Non-Linux builds use a short `try_wait` polling fallback. Remote or multi-observer liveness belongs in a later phase.

## Toolset boundary and compatibility

The leaf `harnx-toolset` crate has no runtime or NATS dependency. Its `Toolset` trait exposes:

```rust
fn name(&self) -> &str;
fn tools(&self) -> Vec<ToolSpec>;
async fn invoke(
    &self,
    tool: &str,
    args: serde_json::Value,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, ToolInvokeError>;
```

`harnx-toolset-server` adapts any implementation to the NATS protocol. Passing `--mcp-stdio` selects its generic MCP stdio adapter for third-party consumers and migration tests. The pilot `TimeToolset` provides `get_current_time`, `convert_time`, `wait`, and `wait_until` through both adapters.

## Worker bootstrap and coexistence

Phase 2a initially used a private built-in bootstrap list (`LOCAL_BOOTSTRAP_SERVERS`). In Phase 2b (slice a), this hardcoded list was replaced with generalized, config-driven tool-server declarations loaded from `tool_servers/*.yaml` (across user config and package directories). The `time` tool server now ships via `tool_servers/time.yaml`, with `HARNX_TIME_SERVER_BIN` retained as a per-server binary override seam for integration testing.

The local worker spawns tool servers lazy-on-demand only when their tool-name prefix could match the active agent's `use_tools` selectors. Servers receive `HARNX_INSTANCE_ID`, `HARNX_NATS_URL`, and `HARNX_NATS_TOKEN`. If a declared tool-server binary is missing or exits prematurely, the worker emits a UI warning (`AgentEvent::Notice(Warning)`) and continues running rather than failing hard. Non-local workers do not start local tool servers, preserving remote `agent@cluster` behavior.

`NatsToolProvider` snapshots registrations into declarations and is ordered before ACP, MCP, and session-history providers. NATS therefore wins a name collision during incremental migration. Tools without NATS registrations continue through their existing stdio provider. The e2e test executes a real `get_current_time` over NATS and `legacy_get_current_time` through the same binary's `--mcp-stdio` adapter in one tool-evaluation batch.

Phase 2b slice a completed replacing the hardcoded bootstrap list with `tool_servers/*.yaml`. Remaining Phase 2b future work (migrating other toolsets, sub-agent NATS integration, and eventually removing stdio) remains open while preserving mixed operation until complete.

## Trust policy and ACP recursion guard

All configured tool and sub-agent children are trusted broker principals in local mode. They inherit `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` by design; `scrub_local_nats_env` and its call sites were removed.

Broker credentials don't determine ACP execution role. ACP children still receive `HARNX_INTERNAL_ACP_ROLE=backend`, which prevents a worker-owned local ACP backend from re-entering front-end NATS orchestration. Backend local refs execute in-process, while `agent@cluster` refs continue through the thin-client path.

## Verification coverage

`time_over_nats_pilot_e2e_mixed_stdio_cancel_and_crash` starts an authenticated JetStream-capable broker and the real pilot through `ToolServerSupervisor`. It verifies:

- registration completes before invocation;
- a real UTC time reply appears in worker `ToolResult` output;
- NATS and MCP stdio calls complete in one evaluation batch;
- abort publishes `ControlMessage::Cancel` on the per-instance control subject and returns in under two seconds;
- killing the pilot during a 30-second wait produces a named recoverable crash error in under two seconds through the process monitor, not the 60-second NATS timeout;
- crash cleanup removes the registration and subsequent discovery omits the dead server.

The test detects `nats-server` through `NATS_SERVER_BIN` first and then `PATH`. It skips cleanly when no broker binary is available.

## Related work

- GitHub issue: [#1224](https://github.com/dobesv/harnx/issues/1224)
- Phase 1: `docs/solutions/nats-local-frontend-backend-split-2026-07-28.md`
- Phase 2b: slice a delivered config-driven bootstrap (`tool_servers/*.yaml`). Remaining work covers other toolsets, sub-agents, and stdio removal.
