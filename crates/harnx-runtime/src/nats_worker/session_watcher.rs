//! Follows a session's own JetStream stream for the duration of a turn.
//!
//! The worker's own stream is the only interrupt authority: a client-issued
//! `Cancel` control command (`nats_worker::control`) is merely a latency hint
//! that is re-verified against the log before it acts. This watcher is what
//! actually reacts to the durable entry, concurrently with whatever the turn
//! is awaiting — it does not wait for the turn to poll anything.
//!
//! A `Cancel` entry this worker did not just append itself (a frontend, a
//! parent session, or a previous worker instance) interrupts: it records an
//! `InterruptNotice`, fires the abort signal, and publishes a tool cancel for
//! every call currently in flight for the session. A `Message` from a user
//! only flags that input is waiting; it never touches the abort signal.
//!
//! The stream cursor survives reconnects: it advances as each message is
//! seen, not only when the consumer runs dry (which an ordered push
//! consumer essentially never does on its own), so a broker hiccup resumes
//! just past the last message actually delivered instead of replaying the
//! whole turn's history. An entry that fails to deserialize is skipped and
//! logged, not treated as a reason to reconnect.
//!
//! The Cancel path awaits only the in-flight snapshot before firing the
//! abort signal; the actual publish fan-out runs on its own detached task so
//! it keeps going even if this watcher is aborted moments later — which is
//! exactly what `execute_session` does as soon as the abort signal wakes it.
//!
//! Manual compaction requests are also detected here. A `CompactRequest`
//! without a matching `CompactResult` sets a pending compaction flag that
//! triggers execution at the post-turn safe boundary. This mirrors the Cancel
//! detection but without firing the abort signal.

use crate::nats_tool_provider::{publish_tool_cancel, NatsInFlightCalls};
use async_nats::jetstream::consumer::{push::OrderedConfig, DeliverPolicy};
use futures_util::StreamExt;
use harnx_core::session::SessionLogEntry;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Everything one session execution's watcher task needs. Built once at
/// activation and moved into the spawned task.
pub(super) struct SessionWatcherCtx {
    pub jetstream: async_nats::jetstream::Context,
    pub client: async_nats::Client,
    /// Storage key, not necessarily the bare frontend-facing session id.
    pub session_id: String,
    /// Tail sequence observed at activation; watching starts just after it.
    pub start_after: u64,
    pub abort_signal: crate::utils::AbortSignal,
    /// Held for the whole session execution: a strong handle keeps the
    /// process-wide per-instance map alive, so tool/hook registrations made
    /// during the turn land where this watcher's snapshot can find them.
    pub in_flight: NatsInFlightCalls,
    /// The `after_seq_observer` high-water mark shared with the log backend:
    /// sequences <= this were written by this worker, not a foreign writer.
    pub own_appends: Arc<AtomicU64>,
    pub pending_input: Arc<AtomicBool>,
    pub interrupted: Arc<Mutex<Option<InterruptNotice>>>,
    /// Pending manual compaction request. Set when a `CompactRequest` is
    /// detected; cleared when worker handles it at safe boundary.
    pub pending_compaction: Arc<Mutex<Option<String>>>,
}

/// Recorded when a foreign `Cancel` interrupts the turn: which log sequence
/// ended it, and the cancellation id threaded through to the tool cancels.
#[derive(Debug, Clone)]
pub(super) struct InterruptNotice {
    pub cancel_seq: u64,
    pub cancellation_id: Option<String>,
}

/// Spawn the per-session watcher task. Runs until aborted by the caller
/// alongside the session's other per-execution tasks (lease-loss watch,
/// control listener).
pub(super) fn spawn_session_watcher(ctx: SessionWatcherCtx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut cursor = ctx.start_after;
        loop {
            match watch_from(&ctx, &mut cursor).await {
                Err(error) => log::debug!(
                    "session watcher reconnecting: session_id={} error={error:#}",
                    ctx.session_id
                ),
                // An ordered push consumer essentially never runs dry on its
                // own, so a clean end is as much of a surprise as an error and
                // gets the same back-off: reopening it the instant it happened
                // would spin this task against the broker at full speed for as
                // long as whatever ended it lasts.
                Ok(()) => log::debug!(
                    "session watcher stream ended; reopening: session_id={}",
                    ctx.session_id
                ),
            }
            tokio::time::sleep(WATCH_REOPEN_DELAY).await;
        }
    })
}

/// How long the watcher waits before opening a new consumer, however the last
/// one ended.
const WATCH_REOPEN_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// Follow the stream from just after `*cursor` until the consumer errors
/// (broker hiccup, missed heartbeat, connection loss). `*cursor` advances as
/// every message is seen, independent of the `Result` this returns — a
/// stack local that only escaped via a successful return would be lost on
/// every real exit here, since those are all `?`, and the caller would then
/// reconnect from the start of this call and replay everything already
/// processed (re-snapshotting and re-cancelling a possibly different
/// in-flight set for a `Cancel` seen twice).
async fn watch_from(ctx: &SessionWatcherCtx, cursor: &mut u64) -> anyhow::Result<()> {
    let stream = ctx
        .jetstream
        .get_stream(crate::nats_session_log::stream_name_for_session(
            &ctx.session_id,
        ))
        .await?;
    let consumer = stream
        .create_consumer(OrderedConfig {
            // A push consumer is delivered to a subject, not pulled; an
            // ordered consumer still needs one of its own; `Default::default`
            // leaves this empty, which the server treats as "no push
            // subject" and then rejects for carrying an idle heartbeat
            // (`consumer idle heartbeat requires a push based consumer`).
            deliver_subject: ctx.client.new_inbox(),
            filter_subject: crate::nats_session_log::subject_for_session(&ctx.session_id),
            deliver_policy: DeliverPolicy::ByStartSequence {
                start_sequence: *cursor + 1,
            },
            ..Default::default()
        })
        .await?;
    let mut messages = consumer.messages().await?;
    while let Some(message) = messages.next().await {
        let message = message?;
        let seq = message
            .info()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .stream_sequence;
        *cursor = seq;
        // A frontend's Cancel always lands at a higher sequence than this
        // worker's last append at the moment it is observed here: the
        // worker's own next append would have conflicted first.
        if seq <= ctx.own_appends.load(Ordering::Relaxed) {
            continue; // our own append, not a foreign interrupt
        }
        let entry = match crate::nats_session_log::deserialize_entry(&message.payload) {
            Ok(entry) => entry,
            Err(error) => {
                // Restarting the consumer over one bad entry would replay
                // everything since `*cursor` and re-fire any Cancel already
                // handled. Skip just this one; `*cursor` already moved past it.
                log::warn!(
                    "session watcher skipping undecodable entry: session_id={} seq={seq} error={error:#}",
                    ctx.session_id
                );
                continue;
            }
        };
        match entry {
            SessionLogEntry::Cancel {
                cancellation_id, ..
            } => {
                ctx.interrupted.lock().replace(InterruptNotice {
                    cancel_seq: seq,
                    cancellation_id: cancellation_id.clone(),
                });
                let id = cancellation_id.unwrap_or_else(|| format!("cancel-{seq}"));
                // Awaits only the in-flight snapshot and the (synchronous)
                // spawn of the publish fan-out below, so both happen before
                // `set_ctrlc` even if this task is aborted the instant that
                // fires — which is exactly what `execute_session` does.
                let sent =
                    cancel_in_flight_calls(&ctx.client, &ctx.in_flight, &ctx.session_id, &id).await;
                ctx.abort_signal.set_ctrlc();
                log::info!(
                    "session interrupted: session_id={} cancel_seq={seq} tool_cancels={sent}",
                    ctx.session_id
                );
            }
            SessionLogEntry::CompactRequest { compaction_id, .. } => {
                handle_compact_request(ctx, compaction_id);
            }
            SessionLogEntry::CompactResult { compaction_id, .. } => {
                handle_compact_result(ctx, compaction_id);
            }
            SessionLogEntry::Message { role, .. } if role.is_user() => {
                ctx.pending_input.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
    Ok(())
}

fn handle_compact_request(ctx: &SessionWatcherCtx, compaction_id: String) {
    // Compaction runs at a safe turn boundary and doesn't abort active work.
    let mut pending = ctx.pending_compaction.lock();
    if pending.is_none() {
        log::info!(
            "session watcher detected pending compaction: session_id={} compaction_id={compaction_id}",
            ctx.session_id
        );
        *pending = Some(compaction_id);
    } else {
        log::debug!(
            "session watcher ignoring duplicate compaction request: session_id={} compaction_id={compaction_id}",
            ctx.session_id
        );
    }
}

fn handle_compact_result(ctx: &SessionWatcherCtx, compaction_id: String) {
    let mut pending = ctx.pending_compaction.lock();
    if pending.as_deref() == Some(compaction_id.as_str()) {
        log::debug!(
            "session watcher clearing resolved compaction: session_id={} compaction_id={compaction_id}",
            ctx.session_id
        );
        *pending = None;
    }
}

/// Snapshot the calls in flight for this session, then hand the actual
/// publish fan-out to a detached task that owns its own clones of
/// everything it needs. The snapshot is awaited to completion before this
/// returns, so it survives the caller's watcher task being aborted right
/// after (an abort only cancels the watcher's own task, never a task it
/// previously spawned); the detached fan-out then survives that same abort
/// landing mid-publish, since it is no longer running inside the watcher at
/// all. Returns the number of targets addressed. A per-target publish
/// failure is logged and does not stop the rest: an orphaned call still has
/// its own journal-driven cancel path, and a retried interrupt would try
/// again.
pub(super) async fn cancel_in_flight_calls(
    client: &async_nats::Client,
    in_flight: &NatsInFlightCalls,
    session_id: &str,
    cancellation_id: &str,
) -> usize {
    let targets = in_flight.snapshot_for_session(session_id).await;
    let count = targets.len();
    let client = client.clone();
    let session_id = session_id.to_string();
    let cancellation_id = cancellation_id.to_string();
    tokio::spawn(async move {
        for target in &targets {
            if let Err(error) =
                publish_tool_cancel(&client, target, &session_id, &cancellation_id).await
            {
                log::debug!(
                    "tool cancel not published: call_id={} error={error:#}",
                    target.call_id
                );
            }
        }
    });
    count
}
