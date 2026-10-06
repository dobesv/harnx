//! Remote-follow stream for AG-UI clients observing a session whose turn
//! is being driven by a remote NATS worker.
//!
//! When a Web UI client opens a promptless `/run` against a session whose
//! local actor doesn't own the run, this module follows durable pending work
//! and the remote worker's advisory stream. Before the first lease it waits
//! within the configured acquisition budget instead of synthesizing RUN_FINISHED.

mod frames;
mod isolation;
mod pending;

#[cfg(test)]
#[path = "ag_ui_remote_follow/isolation_tests.rs"]
mod isolation_tests;
#[cfg(test)]
mod pending_tests;
#[cfg(test)]
pub(crate) use frames::event_frames;
pub(crate) use frames::{event_frames_with_guard, QueuedEvent};
use isolation::RemoteInterruptWatch;
#[cfg(test)]
pub(crate) use pending::terminal_from_history;
use std::time::Duration;

use ag_ui_core::event::Event;
use anyhow::Result;
use bytes::Bytes;
use harnx_core::{event::AgentEventSink, session::SessionLogEntry};
use harnx_runtime::{
    config::Config,
    nats_event_sink::{AdvisoryEnvelope, JetstreamContext, SessionEventStream},
    nats_lease::session_has_active_lease,
};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio_stream::StreamExt as _;

use crate::ag_ui::{GuardedEventStream, UsageContextSnapshot};

use crate::{
    ag_ui::{frame_event, AgUiError, AgUiSink},
    ag_ui_attach::{session_attach_boundary_event, snapshot_event},
    ag_ui_sync::{frame_run_boundary_event, frame_run_error_event, history_warning_event},
    session_actor::SubscribeResult,
};

/// Poll interval for checking lease status and refreshing history.
/// First tick fires immediately, then every 1s.
/// With LEASE_ABSENT_THRESHOLD=5, the effective crash-detection margin
/// is ~4s after the first poll, which is well within the ~30s lease TTL
/// and provides a comfortable buffer before a worker's next renewal.
const LEASE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Number of consecutive lease-absent polls before declaring worker crash.
/// At 1s poll interval, this yields ~4s effective latency (first tick
/// is immediate). Chosen to be well under the ~30s lease TTL, allowing
/// time for transient network issues to resolve before giving up.
pub(crate) const LEASE_ABSENT_THRESHOLD: usize = 5;

pub(crate) const WORKER_LOST_MESSAGE: &str =
    "The worker handling this session stopped without answering. Check the worker log for the underlying failure.";

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RemoteFollowTerminal {
    Finished,
    Error(String),
}

pub(crate) fn remote_terminal_frame(
    terminal: RemoteFollowTerminal,
    thread_id: &str,
    run_id: &str,
) -> Bytes {
    match terminal {
        RemoteFollowTerminal::Finished => {
            Bytes::from(frame_run_boundary_event("RUN_FINISHED", thread_id, run_id))
        }
        RemoteFollowTerminal::Error(message) => {
            Bytes::from(frame_run_error_event(thread_id, run_id, &message))
        }
    }
}

/// Buffer size for frame-forwarding channel.
/// Sends apply backpressure because mapped advisories contain ordered AG-UI lifecycle
/// frames; dropping any frame can invalidate every later frame in the run.
/// Matches the bounded nature of the local broadcast path (which uses 64).
const FRAME_CHANNEL_SIZE: usize = 256;

/// Parameters for deciding whether to follow a remote worker's stream.
///
/// Passed from `ag_ui.rs` to `resolve_event_stream` when the local actor is idle.
pub(crate) struct EventStreamParams<'a> {
    pub(crate) config: &'a Config,
    pub(crate) cluster: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) run_id: &'a str,
    pub(crate) thread_id: &'a str,
    pub(crate) subscription: &'a SubscribeResult,
    pub(crate) eligible: bool,
}

/// Selects remote-follow for a promptless actor with durable pending input or a lease.
/// Returns `None` when caller should use the regular local event stream.
pub(crate) async fn resolve_event_stream(
    params: EventStreamParams<'_>,
) -> Result<Option<GuardedEventStream>, AgUiError> {
    if !params.eligible {
        return Ok(None);
    }
    let pending = params
        .subscription
        .log_entries
        .as_deref()
        .and_then(harnx_core::session_reconstruct::pending_prompt_seq)
        .is_some();
    if !pending && !check_remote_lease(params.config, params.cluster, params.session_id).await? {
        return Ok(None);
    }

    let stream = build_remote_follow_ag_ui_stream(RemoteFollowStreamParams {
        config: params.config,
        cluster: params.cluster,
        session_id: params.session_id,
        run_id: params.run_id,
        thread_id: params.thread_id,
        subscription: params.subscription,
    })
    .await?;
    Ok(Some(stream))
}

/// Checks whether a remote worker holds the lease for this session.
async fn check_remote_lease(
    config: &Config,
    cluster: &str,
    session_id: &str,
) -> Result<bool, AgUiError> {
    crate::ensure_frontend_nats_owner(cluster)
        .await
        .map_err(|err| AgUiError::Internal(format!("NATS unavailable: {err}")))?;
    let jetstream = crate::serve_nats_jetstream(config, cluster)
        .await
        .map_err(|err| AgUiError::Internal(err.to_string()))?;
    session_has_active_lease(&jetstream, session_id)
        .await
        .map_err(|err| AgUiError::Internal(format!("Lease check failed: {err}")))
}

fn last_user_sequence(entries: &[(u64, SessionLogEntry)]) -> u64 {
    harnx_core::session_reconstruct::pending_prompt_seq(entries)
        .or_else(|| harnx_core::session_reconstruct::latest_prompt_seq(entries))
        .unwrap_or(0)
}

struct RemoteFollowStreamParams<'a> {
    config: &'a Config,
    cluster: &'a str,
    session_id: &'a str,
    run_id: &'a str,
    thread_id: &'a str,
    subscription: &'a SubscribeResult,
}

async fn build_remote_follow_ag_ui_stream(
    params: RemoteFollowStreamParams<'_>,
) -> Result<GuardedEventStream, AgUiError> {
    let initial_events = std::iter::once(snapshot_event(params.subscription.snapshot.clone()))
        .chain(
            params
                .subscription
                .history_warnings
                .iter()
                .cloned()
                .map(history_warning_event),
        );
    let initial_frames = initial_events
        .filter_map(|event| {
            frame_event(&event)
                .map_err(|err| log::warn!("failed to serialize initial AG-UI frame: {err}"))
                .ok()
        })
        .collect::<String>();
    let snapshot_frame = (!initial_frames.is_empty()).then(|| Bytes::from(initial_frames));
    let attachment_frames = params
        .subscription
        .log_entries
        .as_deref()
        .map(|entries| {
            super::ag_ui::message_attachment_snapshot_events(&params.subscription.snapshot, entries)
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|event| frame_event(&event).ok().map(Bytes::from))
        .collect();

    build_remote_follow_event_stream(RemoteEventStreamParams {
        config: params.config,
        cluster: params.cluster,
        session_id: params.session_id,
        run_id: params.run_id,
        thread_id: params.thread_id,
        snapshot_frame,
        attachment_frames,
        session_base: params.subscription.session_base.clone(),
    })
    .await
    .map_err(|err| AgUiError::Internal(format!("Remote follow failed: {err}")))
}

async fn build_remote_follow_event_stream(
    params: RemoteEventStreamParams<'_>,
) -> Result<GuardedEventStream> {
    let client = crate::serve_nats_client(params.config, params.cluster).await?;
    let jetstream = crate::serve_nats_jetstream(params.config, params.cluster).await?;
    let event_stream =
        SessionEventStream::attach(jetstream.clone(), client, params.session_id).await?;
    let started_frame = Bytes::from(frame_run_boundary_event(
        "RUN_STARTED",
        params.thread_id,
        params.run_id,
    ));
    let boundary_frame = Bytes::from(super::ag_ui::frame_event(&session_attach_boundary_event(
        event_stream.last_applied_seq(),
    ))?);

    // Compute through_seq from the single history snapshot (avoiding duplicate load).
    // It is both the completion boundary this follow waits for and the
    // sequence an interrupting `Cancel` has to sit above to be this prompt's.
    let through_seq = last_user_sequence(event_stream.history());
    let interrupt_watch = RemoteInterruptWatch::bind(&jetstream, params.session_id, through_seq);

    if let Some(terminal) = pending::terminal_from_history(event_stream.history(), through_seq) {
        // The worker may have finished since the actor's subscription snapshot.
        let hydration_frames = pending::completion_events(
            event_stream.history(),
            params.session_id,
            params.session_base.as_ref(),
        )?
        .into_iter()
        .filter_map(|event| frame_event(&event).ok().map(Bytes::from))
        .collect();
        return Ok(completed_remote_stream(
            [started_frame, boundary_frame],
            params
                .snapshot_frame
                .filter(|_| params.session_base.is_none()),
            hydration_frames,
            remote_terminal_frame(terminal, params.thread_id, params.run_id),
        ));
    }

    let claim_watchdog = if session_has_active_lease(&jetstream, params.session_id).await? {
        None
    } else {
        Some(
            harnx_runtime::nats_session::SessionLeaseWatchdog::with_acquisition_timeout(
                Duration::from_secs(params.config.data.nats_lease_acquisition_timeout_secs),
            ),
        )
    };
    Ok(build_live_follow_stream(LiveFollowParams {
        interrupt_watch,
        event_stream,
        jetstream,
        session_id: params.session_id.to_string(),
        started_frame,
        boundary_frame,
        snapshot_frame: params.snapshot_frame,
        attachment_frames: params.attachment_frames,
        thread_id: params.thread_id.to_string(),
        run_id: params.run_id.to_string(),
        through_seq,
        claim_watchdog,
        session_base: params.session_base,
    }))
}

struct RemoteEventStreamParams<'a> {
    config: &'a Config,
    cluster: &'a str,
    session_id: &'a str,
    run_id: &'a str,
    thread_id: &'a str,
    snapshot_frame: Option<Bytes>,
    attachment_frames: Vec<Bytes>,
    session_base: Option<harnx_core::session::Session>,
}

struct LiveFollowParams {
    interrupt_watch: RemoteInterruptWatch,
    event_stream: SessionEventStream,
    jetstream: JetstreamContext,
    session_id: String,
    started_frame: Bytes,
    boundary_frame: Bytes,
    snapshot_frame: Option<Bytes>,
    attachment_frames: Vec<Bytes>,
    thread_id: String,
    run_id: String,
    through_seq: u64,
    claim_watchdog: Option<harnx_runtime::nats_session::SessionLeaseWatchdog>,
    session_base: Option<harnx_core::session::Session>,
}

fn build_live_follow_stream(params: LiveFollowParams) -> GuardedEventStream {
    let live = params.event_stream.live_state().clone();
    let guard = std::sync::Arc::new(std::sync::Mutex::new(
        crate::ag_ui_lifecycle::LiveStreamGuard::default(),
    ));
    let attached_seq = params.event_stream.last_applied_seq();
    let (tx, rx) = tokio::sync::mpsc::channel(FRAME_CHANNEL_SIZE);

    // Control-state hydration for remote-follow: emit control CUSTOM events after snapshot
    // Use history before spawning the follow task (which takes ownership of event_stream)
    // Note: For live-follow, we don't recompute context here since the session may still be
    // actively running on the remote worker. Context will be computed when the follow
    // transitions to idle and the session is fully reconstructed.
    let control_events = super::ag_ui::control_snapshot_events(params.event_stream.history(), None);
    let control_frames: Vec<Bytes> = control_events
        .into_iter()
        .filter_map(|e| super::ag_ui::frame_event(&e).ok().map(Bytes::from))
        .collect();

    let (terminal_tx, terminal_rx) = oneshot::channel();
    spawn_follow_task(
        FollowTaskParams {
            interrupt_watch: params.interrupt_watch,
            event_stream: params.event_stream,
            jetstream: params.jetstream,
            session_id: params.session_id,
            tx,
            through_seq: params.through_seq,
            claim_watchdog: params.claim_watchdog,
            session_base: params.session_base,
        },
        terminal_tx,
    );

    let initial_frames = vec![params.started_frame, params.boundary_frame]
        .into_iter()
        .chain(params.snapshot_frame)
        .chain(params.attachment_frames)
        .chain(control_frames);
    let event_frames = event_frames_with_guard(rx, live, attached_seq, guard.clone());
    let thread_id = params.thread_id;
    let run_id = params.run_id;
    let terminal_stream = futures::stream::once(async move {
        let terminal = terminal_rx.await.unwrap_or_else(|_| {
            RemoteFollowTerminal::Error("Remote follow task stopped unexpectedly".to_string())
        });
        remote_terminal_frame(terminal, &thread_id, &run_id)
    });

    let stream = Box::pin(
        tokio_stream::iter(initial_frames)
            .chain(event_frames)
            .chain(terminal_stream),
    );
    GuardedEventStream { stream, guard }
}

pub(crate) fn completed_remote_stream(
    initial_frames: [Bytes; 2],
    snapshot_frame: Option<Bytes>,
    control_frames: Vec<Bytes>,
    terminal_frame: Bytes,
) -> GuardedEventStream {
    let frames: Vec<Bytes> = initial_frames
        .into_iter()
        .chain(snapshot_frame)
        .chain(control_frames)
        .chain(std::iter::once(terminal_frame))
        .collect();
    GuardedEventStream {
        stream: Box::pin(tokio_stream::iter(frames)),
        guard: std::sync::Arc::new(std::sync::Mutex::new(
            crate::ag_ui_lifecycle::LiveStreamGuard::default(),
        )),
    }
}

struct FollowTaskParams {
    interrupt_watch: RemoteInterruptWatch,
    event_stream: SessionEventStream,
    jetstream: JetstreamContext,
    session_id: String,
    tx: tokio::sync::mpsc::Sender<QueuedEvent>,
    through_seq: u64,
    claim_watchdog: Option<harnx_runtime::nats_session::SessionLeaseWatchdog>,
    session_base: Option<harnx_core::session::Session>,
}

fn spawn_follow_task(params: FollowTaskParams, terminal_tx: oneshot::Sender<RemoteFollowTerminal>) {
    tokio::spawn(async move {
        let terminal = match remote_follow_task(params).await {
            Ok(terminal) => terminal,
            Err(err) => {
                log::warn!("Remote follow task error: {err:#}");
                RemoteFollowTerminal::Error(format!("Remote follow failed: {err:#}"))
            }
        };
        let _ = terminal_tx.send(terminal);
    });
}

async fn remote_follow_task(params: FollowTaskParams) -> Result<RemoteFollowTerminal> {
    let live = params.event_stream.live_state().clone();
    let interrupt_watch = params.interrupt_watch.clone();
    // Stop observation must stay pollable while history reads or a full output
    // queue hold the follower. Closing the channel settles the wire lifecycle.
    tokio::select! {
        biased;
        result = async move { interrupt_watch.wait_for_stop().await } => {
            // Fence first, then detach: anything still queued from below the
            // `Cancel` is output the interrupt already ended, and the fence
            // outlives this attachment where `retire` does not.
            if let Ok(cancel_seq) = &result {
                live.accept_interrupt(*cancel_seq);
            }
            live.retire();
            result.map(|_| RemoteFollowTerminal::Finished)
        }
        result = follow_remote_turn(params) => result,
    }
}

async fn follow_remote_turn(mut params: FollowTaskParams) -> Result<RemoteFollowTerminal> {
    let tx_for_close = params.tx.clone();
    let mut forwarder = AdvisoryForwarder::new(params.tx);
    let mut poller = RemoteTurnPoller::new(
        params.jetstream,
        params.session_id.clone(),
        params.through_seq,
        params.claim_watchdog,
    );
    let mut lease_poll_interval = tokio::time::interval(LEASE_POLL_INTERVAL);
    lease_poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            envelope = params.event_stream.next() => {
                if !forwarder
                    .forward(&params.event_stream, envelope, &params.session_id)
                    .await
                {
                    break Ok(RemoteFollowTerminal::Finished);
                }
            }
            _ = lease_poll_interval.tick() => {
                match poller.poll(&mut params.event_stream).await {
                    Ok(Some(terminal)) => {
                        let base = params.session_base.as_ref().filter(|_| {
                            poller.claim_watchdog.is_some() && terminal == RemoteFollowTerminal::Finished
                        });
                        pending::hydrate_completion(&tx_for_close, &params.event_stream,
                            &params.session_id, base).await?;
                        break Ok(terminal);
                    },
                    Ok(None) => {}
                    Err(err) => break Err(err),
                }
            }
            _ = tx_for_close.closed() => break Ok(RemoteFollowTerminal::Finished),
        }
    }
}

pub(crate) struct AdvisoryForwarder {
    sink: AgUiSink,
    event_rx: UnboundedReceiver<Event>,
    tx: tokio::sync::mpsc::Sender<QueuedEvent>,
}

impl AdvisoryForwarder {
    pub(crate) fn new(tx: tokio::sync::mpsc::Sender<QueuedEvent>) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let message_id = ag_ui_core::types::ids::MessageId::random();
        Self {
            sink: AgUiSink::new_for_remote_follow(event_tx, message_id),
            event_rx,
            tx,
        }
    }

    async fn forward(
        &mut self,
        event_stream: &SessionEventStream,
        envelope: Option<AdvisoryEnvelope>,
        session_id: &str,
    ) -> bool {
        let Some(envelope) = envelope else {
            log::debug!("Remote advisory subscription closed for session {session_id}");
            return false;
        };
        if !event_stream.should_render(&envelope) {
            return true;
        }
        // Carry the advisory's sequence with the frames it maps to: the queue
        // re-checks it against the fence when they reach the wire, which may be
        // long after an interrupt has landed.
        let after_seq = envelope.after_seq;
        self.sink.emit(envelope.event);
        self.drain_events(after_seq).await
    }

    async fn drain_events(&mut self, after_seq: u64) -> bool {
        while let Ok(event) = self.event_rx.try_recv() {
            let queued = QueuedEvent { after_seq, event };
            if self.tx.send(queued).await.is_err() {
                return false;
            }
        }
        true
    }

    /// Queue one agent event as if an advisory carrying `after_seq` had mapped
    /// to it, so a test can place output either side of an interrupt fence.
    #[cfg(test)]
    pub(crate) async fn forward_agent_event(
        &mut self,
        after_seq: u64,
        event: harnx_core::event::AgentEvent,
    ) -> bool {
        self.sink.emit(event);
        self.drain_events(after_seq).await
    }
}

struct RemoteTurnPoller {
    jetstream: JetstreamContext,
    session_id: String,
    through_seq: u64,
    lease_absent_count: usize,
    claim_watchdog: Option<harnx_runtime::nats_session::SessionLeaseWatchdog>,
    lease_config: harnx_runtime::nats_lease::NatsLeaseConfig,
    lease_bucket: Option<async_nats::jetstream::kv::Store>,
}

impl RemoteTurnPoller {
    fn new(
        jetstream: JetstreamContext,
        session_id: String,
        through_seq: u64,
        claim_watchdog: Option<harnx_runtime::nats_session::SessionLeaseWatchdog>,
    ) -> Self {
        Self {
            jetstream,
            session_id,
            through_seq,
            lease_absent_count: 0,
            claim_watchdog,
            lease_config: harnx_runtime::nats_lease::NatsLeaseConfig::default(),
            lease_bucket: None,
        }
    }

    async fn lease_active(&mut self) -> Result<bool> {
        if self.lease_bucket.is_none() {
            let bucket = harnx_metrics::time_nats_operation(
                "kv_bucket_open",
                self.jetstream.get_key_value(&self.lease_config.bucket),
            )
            .await;
            match bucket {
                Ok(bucket) => self.lease_bucket = Some(bucket),
                Err(error) if harnx_runtime::nats_admin::kv_bucket_missing(&error) => {
                    return Ok(false)
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "Failed to open NATS lease bucket '{}': {error}",
                        self.lease_config.bucket
                    ))
                }
            }
        }
        // Retry opening a missing bucket on the next poll; retain a successful
        // handle so steady-state polls only read the lease key.
        let bucket = self.lease_bucket.as_ref().expect("lease bucket opened");
        match harnx_runtime::nats_lease::lease_holder_in(
            bucket,
            &self.lease_config,
            &self.session_id,
        )
        .await
        {
            Ok(holder) => Ok(holder.is_some()),
            Err(_) => {
                // The bucket may have been deleted/recreated since it was cached.
                self.lease_bucket = None;
                session_has_active_lease(&self.jetstream, &self.session_id).await
            }
        }
    }

    async fn poll(
        &mut self,
        event_stream: &mut SessionEventStream,
    ) -> Result<Option<RemoteFollowTerminal>> {
        let lease_active = self.lease_active().await?;
        let _history_updated = event_stream.refresh_history().await?;
        if let Some(terminal) =
            pending::terminal_from_history(event_stream.history(), self.through_seq)
        {
            return Ok(Some(terminal));
        }
        if lease_active {
            self.claim_watchdog = None;
        } else if let Some(watchdog) = &mut self.claim_watchdog {
            return Ok(watchdog
                .check(&self.jetstream, &self.session_id)
                .await
                .map(RemoteFollowTerminal::Error));
        }
        let terminal = terminal_after_lease_poll(&mut self.lease_absent_count, lease_active);
        if terminal.is_some() {
            log::warn!(
                "Lease absent for {} consecutive polls with no TurnEnd for session {}, reporting worker loss",
                self.lease_absent_count,
                self.session_id
            );
        }
        Ok(terminal)
    }
}

pub(crate) fn terminal_after_lease_poll(
    lease_absent_count: &mut usize,
    lease_active: bool,
) -> Option<RemoteFollowTerminal> {
    if lease_active {
        *lease_absent_count = 0;
        return None;
    }

    *lease_absent_count += 1;
    (*lease_absent_count >= LEASE_ABSENT_THRESHOLD)
        .then(|| RemoteFollowTerminal::Error(WORKER_LOST_MESSAGE.to_string()))
}

impl AgUiSink {
    /// Creates a sink for the remote-follow path.
    ///
    /// Uses an unbounded internal event channel; the bounded wire-frame channel
    /// applies async backpressure without discarding lifecycle events.
    pub(crate) fn new_for_remote_follow(
        tx: UnboundedSender<Event>,
        message_id: ag_ui_core::types::ids::MessageId,
    ) -> Self {
        Self::with_snapshot(tx, message_id, false, None)
    }
}
