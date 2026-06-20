---
title: "False-negative ACP idle timeout during long quiet remote work"
date: 2026-06-18
last_updated: 2026-06-19
category: async-patterns
problem_type: logic_error
component: harnx-acp, harnx-client
root_cause: "missing HTTP read_timeout caused LLM provider stalls to hang indefinitely, surfacing as misleading ACP idle timeout; prior liveness-sentinel fix addressed spurious timeouts during quiet-but-progressing work, but not the deeper transport-layer gap"
resolution_type: code_fix
severity: high
tags:
  - tokio
  - select
  - timeout
  - broadcast
  - acp
  - liveness
  - heartbeat
  - reqwest
  - http-client
  - read-timeout
  - layered-timeout
plan_ref: acp-idle-timeout-874
---

## Problem

`AcpClient::session_prompt` aborted an in-flight prompt with
`"ACP server '<name>' timed out during session/prompt (idle timeout)"` even
when the remote agent was still actively working and subsequently returned a
valid result. A **false negative**: the parent gave up listening while the
remote subprocess kept running and completed the request.

Observed in practice: a delegated review reported the idle-timeout error to the
caller three times, yet the agent had actually produced a complete review and
written its report file.

## Symptoms

- Long delegations (deep reviews, large refactors, builds) intermittently fail
  with "idle timeout" while the remote agent finishes successfully.
- Orchestrators treat completed work as failed, retry unnecessarily, or block a
  mandatory gate that actually passed.
- The bug can self-demonstrate: a review of this very fix tripped the timeout on
  the unpatched caller while the remote review completed and wrote its report.

## Root Cause

In the `tokio::select!` loop of `session_prompt`
(`crates/harnx-acp/src/client.rs`), the idle timer was reset **only** by inbound
`session/update` notifications whose `session_id` matched the active prompt. The
sole emitter of activity was `session_notification`, tagged with that update's
session id.

Long, quiet tool runs (`cargo build`, `cargo nextest`, `git show`, large file
reads) produce no streamed `session/update` chunks. A gap longer than
`idle_timeout_secs` (default 300s) fired the timer even though the agent was
making progress. On timeout the client also returned early **without** sending
`session/cancel`, abandoning still-running remote work.

## Fix

Treat **any** inbound traffic from the subprocess as a liveness signal, not just
session-matched updates.

1. **Generic liveness sentinel.** Broadcast an empty-string session-id sentinel
   (`LIVENESS_SENTINEL`) for liveness that is not tied to a specific session.
   Real ACP session IDs are never empty, so the empty string is an unambiguous
   "still alive" heartbeat.

   ```rust
   const LIVENESS_SENTINEL: &str = "";

   fn signal_liveness(&self) {
       let _ = self.activity_tx.send(LIVENESS_SENTINEL.to_string());
   }
   ```

2. **More liveness sources.** Call `signal_liveness()` from every inbound ACP
   request handler (request_permission, read/write_text_file, create_terminal,
   wait_for_terminal_exit, kill_terminal) and from the subprocess stderr reader
   task.

3. **Pure, testable reset decision.** Extract the select-loop decision into a
   free function so it can be unit-tested without a live subprocess:

   ```rust
   fn idle_activity_resets_timer(
       result: &Result<String, broadcast::error::RecvError>,
       session_id: &str,
   ) -> bool {
       match result {
           Ok(sid) => sid == session_id || sid == LIVENESS_SENTINEL,
           Err(broadcast::error::RecvError::Lagged(_)) => true,  // overflow = activity
           Err(broadcast::error::RecvError::Closed) => false,
       }
   }
   ```

   `Lagged` (broadcast buffer overflow) is itself proof of heavy activity, so it
   resets the timer; `Closed` does not.

4. **Best-effort cancel on timeout.** On both the idle and overall timeout arms,
   enqueue `CancelSession` fire-and-forget (no await on its response) so cleanup
   adds no latency before the error is surfaced:

   ```rust
   async fn request_session_cancel_best_effort(&self, session_id: &str) {
       let Ok(tx) = self.worker_sender().await else { return };
       let (respond_to, _response_rx) = oneshot::channel();
       let _ = tx.send(WorkerCommand::CancelSession {
           session_id: session_id.to_owned(),
           respond_to,
       });
   }
   ```

5. **Softer error.** The idle-timeout message now hints "the remote agent may
   still be running".

## Why This Works

- Quiet periods are normal for deep work; resetting on any subprocess liveness
  (requests, stderr, broadcast overflow) keeps the timer alive whenever the
  agent is making progress, while still catching genuinely hung sessions.
- The empty-string sentinel is safe because ACP session IDs are non-empty; a
  named constant + helper makes the invariant explicit rather than implicit.
- Fire-and-forget cancellation stops abandoned remote work without making the
  caller wait a second timeout.


## Follow-up: Deeper Root Cause — Missing HTTP Read Timeout

The prior fix (liveness sentinel resetting the ACP idle timer on stderr/inbound-request heartbeats) addressed **spurious** idle timeouts during quiet-but-progressing work, but the issue recurred. Investigation revealed a deeper transport-layer gap.

### Symptoms (Recurring)

- ACP idle timeout fires even when the agent subprocess is healthy and making no requests.
- No stderr activity, no session/update, no inbound requests — complete radio silence.
- Liveness heartbeats never fire because the subprocess itself is stalled waiting on a hung LLM provider call.

### Root Cause

The LLM HTTP client (`crates/harnx-client/src/client.rs::build_client`) set **only** `connect_timeout` (10s) — **no read/overall timeout**. A stalled LLM provider (TCP/TLS connected but sending no bytes, common when streaming SSE hangs upstream) hangs **indefinitely**. The agent subprocess then goes fully silent (no stderr, no session/update, no inbound requests), so liveness heartbeats never fire and the parent's ACP idle timer trips with a misleading "idle timeout" error.

**The idle timeout was the symptom; the missing LLM read timeout was the cause.**

### Solution

1. **Add `read_timeout` to HTTP client.** Added `read_timeout: Option<u64>` to `ExtraConfig` (harnx-core/src/api_types.rs); `build_client` applies reqwest `.read_timeout(120s default)`.

2. **Correct timeout semantics.** reqwest `.read_timeout()` is a **per-read inactivity** timeout (fires only when no bytes arrive for the window), so it does **not** kill long-but-progressing streaming responses — only true stalls. This is why `.read_timeout()` is correct here and a total `.timeout()` would be **wrong** (it would kill healthy long generations).

3. **Coerce zero to default.** `0` must be coerced to the default because reqwest treats a 0-duration timeout as infinite (silently disabling protection). Added a `resolve_timeout_secs` helper:

   ```rust
   fn resolve_timeout_secs(configured: Option<u64>, default: u64) -> u64 {
       // reqwest treats 0 as infinite timeout; coerce it back to default.
       configured.filter(|&secs| secs > 0).unwrap_or(default)
   }
   ```

4. **Raise ACP backstop.** Raised ACP `idle_timeout_secs` default 300→600s, documented as a backstop now that the LLM layer fails fast.

### Why This Works

- Layered timeouts catch distinct failure modes:
  - **HTTP read_timeout (120s)**: Primary — catches stalled provider reads, surfaces as provider/API error.
  - **ACP idle_timeout (600s)**: Backstop — catches fully silent subprocess hangs (e.g., process wedged before making HTTP call).
  - **operation_timeout (3600s)**: Hard ceiling — total session lifetime wall.

- Per-read inactivity semantics ensure healthy long streams continue; only true dead air triggers.
- Error attribution now distinguishes: "Failed to call chat-completions api ... operation timed out" vs "ACP server idle timeout".

### Known Gap

`LlamaServerClient` uses hyper over a Unix socket, bypassing reqwest, so it ignores `read_timeout`. A hung local llama-server still falls through to the ACP backstop. This is a known remaining gap and the subject of a follow-up issue.

### Generalizable Lesson

**Layered timeouts are essential.** A missing transport-layer timeout surfaces as a misleading higher-layer timeout. When diagnosing "misleading timeout" bugs, check each layer from bottom (transport) to top (process/application).

## Prevention Strategies

**Code Review Checklist:**
- [ ] Idle/heartbeat timers reset on *any* liveness signal, not only
      narrowly-matched events.
- [ ] Timeout branches that abandon remote work also request cancellation.
- [ ] Cancellation on a timeout path is best-effort (no second blocking await).
- [ ] Magic sentinels are named constants with a documented invariant.
- [ ] All HTTP clients have both connect and read timeouts configured.
- [ ] Read timeouts use per-read inactivity semantics (not total request deadline) for streaming workloads.

**Testability:**
- Extract `select!`-arm decision logic into pure functions and unit-test the
  truth table (matching id, sentinel, unrelated id, `Lagged`, `Closed`).
- For the timeout path itself, inject a worker handle and a short timeout, then
  assert the error text and that a `CancelSession` command was enqueued.
- For HTTP timeout configuration, verify deserialization and that zero values are
  coerced to defaults (per `resolve_timeout_secs`).

## Related Issues

- **GitHub:** [issue #874](https://github.com/dobesv/harnx/issues/874) — ACP
  session_prompt aborts with false 'idle timeout' while remote agent is still
  working.
- **Related Solution:**
  [acp-io-task-supervision-2026-05-07.md](acp-io-task-supervision-2026-05-07.md)
  — supervision patterns for ACP subprocess I/O tasks.
