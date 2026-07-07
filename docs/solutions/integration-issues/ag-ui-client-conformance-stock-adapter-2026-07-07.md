---
title: "AG-UI stock @ag-ui/client adapter conformance requirements"
date: 2026-07-07
category: "integration-issues"
problem_type: integration_issue
component: "harnx-serve, web client"
root_cause: "stock @ag-ui/client adapter has hardcoded verifyEvents that rejects off-spec event ordering, UUID-only IDs, and unterminated runs"
resolution_type: code_fix
severity: high
tags:
  - ag-ui
  - sse
  - assistant-ui
  - verifyEvents
  - runId-echo
  - FirstRunState
  - stock-adapter
plan_ref: "harnx-webui-first-version"
---

## Problem

The stock `@ag-ui/client` adapter has hardcoded `verifyEvents` validation with no disable flag. A server emitting `MESSAGES_SNAPSHOT` before `RUN_STARTED`, using UUID-only ID parsing, or failing to echo the client's `runId` in boundary frames causes the adapter to reject the stream with a 400/invalid event error. This blocked the first-version React web client from using the stock assistant-ui runtime without client-side hacks.

## Symptoms

- **Event ordering rejection**: Server emitting `MESSAGES_SNAPSHOT` first causes `verifyEvents` to reject — error: "expected RUN_STARTED as first event"
- **ID parsing 400**: Server using `ag-ui-core` UUID-newtype parsers returns 400 for nanoid RunId/ThreadId from client (adapter generates nanoids, server expects UUID)
- **runId mismatch**: Server generating its own `runId` causes `verifyEvents` to emit "runId mismatch in RUN_FINISHED" warning/event
- **Unterminated join**: Promptless SSE join (history load only) leaving a synthetic `RUN_STARTED` open causes client `isRunning` to stick true, blocking compose
- **RUN_ERROR + RUN_FINISHED double-emit**: Server emitting RUN_FINISHED after RUN_ERROR fails `verifyEvents` — RUN_ERROR is terminal

## Investigation Steps

1. Started with stock `@assistant-ui/react` + `@assistant-ui/react-ag-ui` + `@ag-ui/client`. Adapter connected but immediately rejected the SSE stream.
2. Read `@ag-ui/client` source: found `verifyEvents` is hardcoded, no skip flag. Event validation rules:
   - First event MUST be `RUN_STARTED`
   - `RUN_STARTED`/`RUN_FINISHED` carry `runId` — client correlates by the EXACT `runId` it sent in the POST body
   - `RUN_ERROR` is terminal; no `RUN_FINISHED` follows it
   - Multiple sequential runs on one connection allowed: `verifyEvents` resets after `RUN_FINISHED`, ready for next `RUN_STARTED`
3. Tested with curl: confirmed server was emitting `MESSAGES_SNAPSHOT` first (from session-actor), no `RUN_STARTED` envelope.
4. Attempted relaxed ID parsing using raw string IDs instead of UUID parsing — still failed because `verifyEvents` expects the ECHO of client's nanoid `runId`.
5. Realized the fix must be server-side, localized to the SSE framing logic, not a client hack (client-hack encodes non-conformance forever).
6. Oracle-guided design: "preamble + passthrough with first-run-only substitution" — emit synthetic `RUN_STARTED` with client's `runId`, suppress/filter the first run's session-actor boundaries, then passthrough all subsequent live events untouched.

## Root Cause

The `@ag-ui/client` adapter's `verifyEvents` function enforces:

1. **RUN_STARTED first**: Event stream must begin with `RUN_STARTED`, not `MESSAGES_SNAPSHOT`. Server was delegating to session-actor which emits snapshot first.
2. **runId correlation**: `RUN_STARTED` and `RUN_FINISHED` MUST carry the SAME `runId` the client sent in `RunAgentInput`. Server was generating its own server-authoritative `runId`.
3. **String IDs tolerated**: `verifyEvents` treats IDs as opaque strings — doesn't parse them. But server-side `ag-ui-core` types (`ThreadId`, `RunId`) are UUID newtypes with strict parsing.
4. **RUN_ERROR is terminal**: No `RUN_FINISHED` after `RUN_ERROR`. Server was emitting both.
5. **Unterminated runs block compose**: A promptless history-load join must emit a closed run (`RUN_STARTED` → `MESSAGES_SNAPSHOT` → `RUN_FINISHED`) to clear `isRunning`. An orphan `RUN_STARTED` leaves the adapter in a running state.

## Solution

### 1. Relaxed Input Parsing (server-side)

Parse incoming `RunAgentInput` with relaxed types that accept arbitrary strings for IDs:

```rust
// Relaxed types for client input (accepts nanoids)
#[derive(Deserialize)]
struct RelaxedRunAgentInput<TState = JsonValue> {
    threadId: String,       // accepts any string, not just UUID
    runId: String,          // client-generated nanoid
    messages: Vec<RelaxedAgUiMessage>,
    #[serde(default)]
    state: TState,
    #[serde(default)]
    tools: Vec<JsonValue>,
    #[serde(default)]
    context: Vec<JsonValue>,
    #[serde(default)]
    forwardedProps: JsonValue,
}

#[derive(Deserialize)]
enum RelaxedAgUiMessage {
    User { id: String, content: String, #[serde(default)] name: Option<String> },
    Assistant { content: String, #[serde(default)] name: Option<String> },
    System { content: String },
    Developer { content: String },
    Tool { content: String },
}
```

Convert relaxed → strict internal types after parsing.

### 2. FirstRunState: Preamble + Passthrough

State machine to emit a leading `RUN_STARTED` envelope, substitute/suppress the FIRST run's boundaries, then passthrough:

```rust
#[derive(Clone, Copy)]
enum FirstRunState {
    AwaitingStarted,  // haven't seen session-actor's RUN_STARTED yet
    Active,           // first run in progress
    Complete,         // first run finished (passthrough all subsequent)
    Errored,         // first run errored (passthrough all subsequent)
}

fn frame_live_event(
    event: Event,
    state: &mut FirstRunState,
    thread_id: &str,
    run_id: &str,    // client's runId, echoed in boundaries
) -> Option<Bytes> {
    match *state {
        FirstRunState::AwaitingStarted => match event {
            Event::RunStarted(_) => {
                *state = FirstRunState::Active;
                None    // suppress session-actor's RUN_STARTED; we already emitted ours
            }
            Event::RunFinished(_) => {
                *state = FirstRunState::Complete;
                Some(Bytes::from(frame_run_boundary_event("RUN_FINISHED", thread_id, run_id)))
            }
            Event::RunError(err) => {
                *state = FirstRunState::Errored;
                Some(Bytes::from(frame_run_error_event(thread_id, run_id, &err.message)))
            }
            other => frame_event(&other).ok().map(Bytes::from),
        },
        FirstRunState::Active => match event {
            Event::RunStarted(_) => None,   // suppress duplicates (shouldn't happen)
            Event::RunFinished(_) => {
                *state = FirstRunState::Complete;
                Some(Bytes::from(frame_run_boundary_event("RUN_FINISHED", thread_id, run_id)))
            }
            Event::RunError(err) => {
                *state = FirstRunState::Errored;
                Some(Bytes::from(frame_run_error_event(thread_id, run_id, &err.message)))
            }
            other => frame_event(&other).ok().map(Bytes::from),
        },
        FirstRunState::Complete | FirstRunState::Errored => {
            // After first run: passthrough everything untouched (real ids)
            frame_event(&event).ok().map(Bytes::from)
        }
    }
}
```

### 3. Boundary Frame with serde_json (NOT raw format!)

Use `serde_json::json!` for boundary framing — raw format! string interpolation is a JSON/SSE injection vector:

```rust
fn frame_run_boundary_event(event_type: &str, thread_id: &str, run_id: &str) -> String {
    let body = serde_json::json!({
        "type": event_type,
        "threadId": thread_id,
        "runId": run_id,
    });
    format!("data: {body}\n\n")
}

fn frame_run_error_event(thread_id: &str, run_id: &str, message: &str) -> String {
    let body = serde_json::json!({
        "type": "RUN_ERROR",
        "threadId": thread_id,
        "runId": run_id,
        "message": message,
    });
    format!("data: {body}\n\n")
}
```

**Security note**: Raw `format!("data: {{\"runId\": \"{}\"}}\n\n", run_id)` with arbitrary client input allows JSON injection (e.g., `run_id = "x\",\"injected\":true}"`) or SSE record splitting (`run_id = "x\ndata: injected-event\n"`). `serde_json::json!` escapes strings correctly.

### 4. Synthetic Closed Run for Promptless Join

For history-only joins (no user prompt), emit a closed run so client `isRunning` clears:

```rust
// No prompt: synthetic closed run + keepalive passthrough
let synthetic = tokio_stream::iter([
    start_frame,      // RUN_STARTED with client's runId
    snapshot_frame,   // MESSAGES_SNAPSHOT
    finish_frame,     // RUN_FINISHED with same runId
]);
// Then chain live passthrough for subsequent RPC-driven runs
let event_stream = synthetic.chain(live_stream.map(|event| frame_event(&event)));
```

### 5. Per-Session HttpAgent (client-side)

Each session gets its own `HttpAgent` with the full session path in the URL:

```tsx
// ChatProvider.tsx
export const ChatProvider: React.FC<ChatProviderProps> = ({ agentName, sessionId, children }) => {
  const agent = useMemo(() => {
    return new HttpAgent({
      url: `/v1/agents/${encodeURIComponent(agentName)}/sessions/${encodeURIComponent(sessionId)}`
    });
  }, [agentName, sessionId]);

  const runtime = useAgUiRuntime({ agent });

  return (
    <AssistantRuntimeProvider key={`${agentName}:${sessionId}`} runtime={runtime}>
      {children}
    </AssistantRuntimeProvider>
  );
};
```

**Key points**:
- `HttpAgent` POSTs verbatim to its `url` — no path segment appending
- `key={agentName:sessionId}` forces React remount on session switch — cleans old adapter state
- Adapter owns send-path (Composer → SSE POST); cancel is a separate JSON-RPC `/rpc` POST

## Why This Works

1. **verifyEvents satisfied**: Stream starts with `RUN_STARTED`, boundaries echo client's `runId`, first run terminates cleanly.
2. **Single SSE connection, multiple runs**: After first run completes, `FirstRunState::Complete` passes all subsequent events through untouched — same connection handles adapter-run + later RPC-driven runs.
3. **Stock adapter, no client hacks**: All conformance fixes are server-side. Client uses stock `@assistant-ui/react`, `@assistant-ui/react-ag-ui`, `@ag-ui/client`.
4. **Server-authoritative threadId tolerated**: Client sends its preferred `threadId`, server derives its own from URL session. `verifyEvents` only correlates `runId` — threadId mismatch is benign (empirically verified).
5. **Injection-safe**: `serde_json::json!` prevents JSON/SSE injection from arbitrary client-id strings.

## Prevention Strategies

### Test Cases

- **Event ordering**: Assert `RUN_STARTED` before `MESSAGES_SNAPSHOT` in SSE stream
- **runId echo**: Assert `RUN_STARTED.runId == RUN_FINISHED.runId == request.runId`
- **RUN_ERROR terminal**: Assert no `RUN_FINISHED` emitted after `RUN_ERROR`
- **ID injection**: Send `runId` with JSON/SSE metacharacters, assert single SSE record and no injection
- **Promptless join**: Assert synthetic `RUN_STARTED → MESSAGES_SNAPSHOT → RUN_FINISHED` sequence
- **Multiple runs**: Assert second `RUN_STARTED` passthrough after first run completes

### Best Practices

- **Never use raw format! for SSE/JSON framing** — always use `serde_json::json!` or typed serialization
- **Echo client's runId** — adapter correlates by the exact ID it sent
- **Promptless join = closed run** — unterminated `RUN_STARTED` blocks client compose
- **RUN_ERROR is terminal** — never emit `RUN_FINISHED` after it
- **FirstRunState pattern** — scope substitution to first run only; passthrough thereafter
- **Kill stale servers before rebuilding** — stale binary on :8000 causes false "nanoid 400" artifacts

### Code Review Checklist

- [ ] SSE stream starts with `RUN_STARTED`?
- [ ] `RUN_STARTED.runId` matches client's request?
- [ ] `RUN_FINISHED.runId` matches `RUN_STARTED.runId`?
- [ ] No `RUN_FINISHED` after `RUN_ERROR`?
- [ ] Promptless join emits closed run?
- [ ] Boundary frames use `serde_json::json!`, not `format!`?
- [ ] `FirstRunState` passthrough works for multiple sequential runs?
- [ ] Client test uses fresh server build (no stale binary)?

## Related Issues

- **Plan**: `harnx-webui-first-version` — React web client implementation
- **Prior solution**: `ag-ui-server-protocol-integration-2026-07-04.md` — server-side AG-UI integration (UUID-newtype ID handling, single-fork resume)
- **Related**: `stateful-event-sink-lifetime-patterns-2026-07-05.md` — AG-UI event sink state machine patterns
- **Issue**: [#959](https://github.com/dobesv/harnx/issues/959) — Web UI first version
- **Commit**: `37895aa5d2` — fix(serve): AG-UI conformance for stock @ag-ui/client adapter (T0)
