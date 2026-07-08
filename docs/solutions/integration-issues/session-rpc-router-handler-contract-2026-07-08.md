---
title: "Session-scoped RPC router/handler contract mismatch caused hidden 404s"
date: 2026-07-08
category: "integration-issues"
problem_type: integration_issue
component: "harnx-serve"
root_cause: "Router path matcher checked wrong segment count, dispatching to wrong handler"
resolution_type: code_fix
severity: high
tags:
  - routing
  - http-dispatch
  - integration-testing
  - router-handler-contract
  - ag-ui
plan_ref: "harnx-ag-ui-followups"
---

# Session-scoped RPC router/handler contract mismatch caused hidden 404s

## Problem

The web UI POSTs session-scoped RPC paths (`/v1/agents/{agent}/sessions/{session}/rpc`) for prompt/cancel operations, but the top-level router's `is_agent_rpc_path` matcher only recognized agent-level 2-segment paths (`[agent, "rpc"]`). RPC requests fell through to `handle_agent_tree`, which returned 404. The bug hid for weeks because existing tests called the RPC handler directly, bypassing the real HTTP dispatch layer.

## Symptoms

```
POST /v1/agents/coding%2Fcoder/sessions/thread-123/rpc
→ HTTP/1.1 404 Not Found
→ {"error":{"message":"Not Found","type":"invalid_request_error"}}

POST /v1/agents/hephaestus/sessions/thread-1/rpc
→ Same 404 for non-encoded paths
```

- All web UI prompt/cancel operations failed silently
- RPC handler tests passed (direct invocation)
- Integration tests passed (bypassed dispatch)
- Issues: web UI send failures appeared as network errors, not server bugs

## Investigation Steps

1. Traced web client `api.ts:34,84` — confirmed POST to `/agents/{agent}/sessions/{session}/rpc`
2. Inspected `harnx-serve/src/lib.rs:204-214` — top dispatch flow:
   - First check: `is_agent_rpc_path(path)` → call `handle_ag_ui_rpc`
   - Second check: `is_session_attachments_path(path)` → attachments handler
   - Default: `/v1/agents/...` → `handle_agent_tree`
3. Found `is_agent_rpc_path` at L817-825 matched only `[_agent, "rpc"]` (2 segments)
4. Verified `parse_agents_route` L781-796 had no arm for `[agent, "sessions", session, "rpc"]`
5. Traced `handle_agent_tree` → `parse_agents_route` returns `None` → HTTP 404
6. RPC handler `ag_ui_rpc.rs:518-532` expected session-scoped paths, but dispatch never reached it
7. Ran probe: `curl -X POST .../sessions/thread-1/rpc` → confirmed 404 against real server
8. Checked existing test helper: `ag_ui_control_plane.rs:125-139` called `handle_ag_ui_rpc_bytes` directly

## Root Cause

**Router/handler contract mismatch.** The HTTP router's dispatch logic and the RPC handler's path parser had different expectations:

- **Router**: `is_agent_rpc_path` checked for `[_agent, "rpc"]` (agent-level only)
- **Handler**: `parse_rpc_path` expected `[agent, "sessions", session, "rpc"]` (session-scoped)

The router selected `handle_agent_tree` for session-scoped RPC paths because the matcher returned false. That handler's `parse_agents_route` didn't recognize the 4-segment RPC shape, returning `None` and triggering 404.

**Why tests missed it**: Existing RPC tests used helper functions that called handlers directly with constructed paths. They validated handler logic, not router dispatch. MSW-based Playwright tests intercepted at the browser level, never hitting the real server.

## Solution

Updated `is_agent_rpc_path` to match both agent-level and session-scoped RPC shapes:

```rust
// Before (harnx-serve/src/lib.rs:817-825)
fn is_agent_rpc_path(segments: &[&str]) -> bool {
    matches!(segments, [_agent, "rpc"])
}

// After
fn is_agent_rpc_path(segments: &[&str]) -> bool {
    matches!(segments, [_agent, "rpc"] | [_agent, "sessions", _session, "rpc"])
}
```

Added regression test verifying router dispatch for session-scoped RPC:

```rust
#[test]
fn server_handle_routes_session_scoped_rpc_requests_to_rpc_handler() {
    let path = "/v1/agents/test-agent/sessions/test-session/rpc";
    let segments: Vec<&str> = path.strip_prefix("/v1/agents/")
        .unwrap()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    assert!(is_agent_rpc_path(&segments));
}
```

## Why This Works

The router now recognizes 4-segment session-scoped RPC paths and dispatches to `handle_ag_ui_rpc`. The handler's `parse_rpc_path` already expected this shape, so the fix required only updating the router matcher. Agent-level RPC (`/v1/agents/{agent}/rpc`) still works via the same matcher arm.

## Prevention Strategies

### Test Cases

- **Integration test at dispatch layer**: Every route shape must be tested through actual `Server::handle` or equivalent entrypoint, not just handler functions
- **Path-shape coverage matrix**: Document all expected path shapes and verify each reaches the correct handler
- **Real-server smoke test**: Run a lightweight HTTP test against actual server routes, not mocks

### Best Practices

- **Router/handler contract tests**: If a handler expects a path shape, the router test must verify dispatch reaches it
- **Avoid direct handler calls in integration tests**: Call through HTTP dispatch, or explicitly test both paths
- **Contract documentation**: Comment path shape expectations at both router and handler definitions

### Code Review Checklist

- [ ] Does router matcher cover all shapes the handler parses?
- [ ] Do integration tests exercise actual HTTP dispatch, not just handler logic?
- [ ] Are path shape expectations documented at both routing and handling layers?
- [ ] When adding a new path shape, did you update both router matcher and add dispatch-level test?

## Related Issues

- **GitHub Issues**: [#985](https://github.com/dobesv/harnx/issues/985) — Percent-decode agent names
- **Related Solution**: [ag-ui-tool-approval-interrupt-resume-2026-07-08.md](./ag-ui-tool-approval-interrupt-resume-2026-07-08.md) — HITL interrupt/resume
msw

## Additional Learnings

### Percent-decoding in raw Hyper servers

Agent names like `coding/coder` are encoded as `coding%2Fcoder` by web clients. Raw Hyper path strings contain the encoded form. Unlike frameworks with middleware decoding, a raw Hyper server must percent-decode agent/session segments manually:

```rust
fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let (Some(h1), Some(h2)) = (chars.next(), chars.next()) {
                if let (Some(v1), Some(v2)) = (hex_val(h1), hex_val(h2)) {
                    bytes.push((v1 << 4) | v2);
                    continue;
                }
            }
        }
        bytes.extend(c.to_string().as_bytes());
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
```

**Key insight**: Only decode the variable segments (agent name, session ID). Route keywords like `"sessions"`, `"rpc"`, `"attachments"` must remain literal for matching.

### MSW v2 auto-decodes URL parameters

MSW v2 internally decodes `%2F` before applying Express-style path matching. A handler for `/v1/agents/:agent/sessions` automatically matches `coding%2Fcoder` and captures `params.agent` as `"coding/coder"`. No custom regex required.

### TypeScript `tsc -b` caching hides unused variable errors

Running `tsc -b` uses incremental `.tsbuildinfo` caches. Unused variable errors (`noUnusedLocals`) can slip past if earlier builds were cached. Run `pnpm build` fresh (or clear `.tsbuildinfo`) to catch these errors during verification.

### Pre-fix proof pattern

To prove error-state tests actually catch bugs:
1. Restore pre-fix code temporarily (`git checkout <pre-fix-sha>`)
2. Run the new tests — observe failures (timeouts, elements not found)
3. Restore fixed code
4. Run tests — observe passes
5. Document observation in PR description; do NOT keep permanent failing tests

This proves the test suite catches the regression without committing hardcoded failing specs.

### Environment notes

- **pnpm required**: Web uses `pnpm` (not npm); CI must use `pnpm install --frozen-lockfile`
- **cargo nextest**: Rust tests use `cargo nextest run`, not `cargo test` (AGENTS.md mandate)
- **Changesets**: User-visible changes require `.changeset/*.md` with key `harnx`
