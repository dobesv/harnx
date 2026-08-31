---
title: "NATS session-log remote-session-migration tests flake under stress"
date: 2026-08-30
last_verified: 2026-08-30
component: "harnx-runtime/src/nats_worker"
problem_type: test_failure
status: current
anchors:
  - crates/harnx-runtime/src/nats_session_log.rs:243-255
  - crates/harnx-runtime/src/nats_worker/tests.rs
tags:
  - NATS
  - JetStream
  - stress-flake
  - remote-session
  - direct-get
plan_ref: "harnx-healthz-addr"
---

# NATS session-log remote-session-migration tests flake under stress

## When this is relevant

Running `cargo nextest run --workspace --stress-count=5` produces intermittent
failures in `remote_rewind_appends_mutation_without_truncating_stream` or
`remote_edit_preserves_canonical_transcript_messages`. Fails under concurrent
stress load (~42s timeout), passes in isolation (<1s). Look here if you see
"must return final assistant response" panics in the remote-session-migration
test family.

## Durable lesson

`nats_session_log.rs:243-255` has an `Ok(Err(_)) => continue` branch in the
JetStream direct-get loop that silently skips a sequence on transient broker
errors under load. When the stress count causes concurrent tests to contend for
JetStream resources, this skip-branch fires intermittently, causing the test's
assertion chain to miss the expected final assistant response.

The skip is intentional (missing sequence due to retention/limits), but it
covers transient errors that should arguably be retried or logged more loudly.
This code predates this feature branch (June commits 1cf12b18/305620e8) and
is unrelated to healthz wiring.

## Evidence and current anchors

- `crates/harnx-runtime/src/nats_session_log.rs:243-255` — the silent skip branch:
  ```rust
  Ok(Err(_)) => continue,  // skip missing sequences rather than failing
  ```
- `crates/harnx-runtime/src/nats_worker/tests.rs:2126` — panic point
- `crates/harnx-runtime/src/nats_worker/tests.rs:323` — passes `None` for readiness,
  proving healthz code path never executes during the flake
- Plan note `652a8c09` — full root-cause analysis showing the flake is pre-existing

## Failed approaches or trade-offs

- **Hardening the skip branch:** Would require distinguishing transient broker
  errors from genuine retention gaps. Currently lumped together. Out of scope
  for this feature; could be a separate hardening effort.
- **Increasing test timeouts:** Bounds the flake but doesn't fix the underlying
  discard-on-error behavior.

## Recommended actions if flake blocks merge

1. Run the failing test in isolation (`cargo nextest run -p harnx-runtime -- <test_name>`)
   to confirm pass.
2. Re-run the stress suite; flakes are non-deterministic and may move between
   siblings in the same family.
3. If flake persists after merge, file separate issue for the discard-on-error
   read path hardening.
