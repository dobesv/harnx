---
title: "False-negative ACP idle timeout during long quiet remote work"
date: 2026-06-18
category: async-patterns
problem_type: logic_error
component: harnx-acp
root_cause: "session_prompt idle timer reset only on session-matched session/update notifications; quiet long tool runs trip a spurious timeout"
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

## Prevention Strategies

**Code Review Checklist:**
- [ ] Idle/heartbeat timers reset on *any* liveness signal, not only
      narrowly-matched events.
- [ ] Timeout branches that abandon remote work also request cancellation.
- [ ] Cancellation on a timeout path is best-effort (no second blocking await).
- [ ] Magic sentinels are named constants with a documented invariant.

**Testability:**
- Extract `select!`-arm decision logic into pure functions and unit-test the
  truth table (matching id, sentinel, unrelated id, `Lagged`, `Closed`).
- For the timeout path itself, inject a worker handle and a short timeout, then
  assert the error text and that a `CancelSession` command was enqueued.

## Related Issues

- **GitHub:** [issue #874](https://github.com/dobesv/harnx/issues/874) — ACP
  session_prompt aborts with false 'idle timeout' while remote agent is still
  working.
- **Related Solution:**
  [acp-io-task-supervision-2026-05-07.md](acp-io-task-supervision-2026-05-07.md)
  — supervision patterns for ACP subprocess I/O tasks.
