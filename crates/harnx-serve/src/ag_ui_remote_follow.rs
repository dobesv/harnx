//! Remote-follow stream for AG-UI clients observing a session whose turn
//! is being driven by a remote NATS worker.
//!
//! When a Web UI client opens a promptless `/run` against a session whose
//! local `SessionActor` is `Idle` but a remote worker holds the lease,
//! this module follows the remote worker's advisory event stream instead of
//! terminating immediately with a synthetic `RUN_FINISHED`.

use std::{pin::Pin, sync::Arc, time::Duration};

use ag_ui_core::event::Event;
use anyhow::Result;
use bytes::Bytes;
use harnx_core::{event::AgentEventSink, session::SessionLogEntry};
use harnx_runtime::{
    config::{Config, LOCAL_CLUSTER_KEY},
    nats_event_sink::{AdvisoryEnvelope, JetstreamContext, SessionEventStream},
    nats_lease::session_has_active_lease,
};
use tokio::sync::{
    mpsc::{error::TrySendError, UnboundedReceiver, UnboundedSender},
    Notify,
};
use tokio_stream::{Stream, StreamExt as _};

use crate::{
    ag_ui::{frame_event, snapshot_event, AgUiError, AgUiSink},
    ag_ui_sync::{frame_run_boundary_event, history_warning_event},
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
const LEASE_ABSENT_THRESHOLD: usize = 5;

/// Buffer size for frame-forwarding channel.
/// Advisory frames are loss-tolerant; a slow client dropping frames is acceptable.
/// Matches the bounded nature of the local broadcast path (which uses 64).
const FRAME_CHANNEL_SIZE: usize = 256;

type AgUiEventStream = Pin<Box<dyn Stream<Item = Bytes> + Send + Sync + 'static>>;

/// Parameters for deciding whether to follow a remote worker's stream.
///
/// Passed from `ag_ui.rs` to `resolve_event_stream` when the local actor is idle.
pub(crate) struct EventStreamParams<'a> {
    pub(crate) config: &'a Config,
    pub(crate) session_id: &'a str,
    pub(crate) run_id: &'a str,
    pub(crate) thread_id: &'a str,
    pub(crate) subscription: &'a SubscribeResult,
    pub(crate) eligible: bool,
}

/// Selects remote-follow for a promptless idle actor with an active remote lease.
/// Returns `None` when caller should use the regular local event stream.
pub(crate) async fn resolve_event_stream(
    params: EventStreamParams<'_>,
) -> Result<Option<AgUiEventStream>, AgUiError> {
    if !params.eligible {
        return Ok(None);
    }
    if !check_remote_lease(params.config, params.session_id).await? {
        return Ok(None);
    }

    let stream = build_remote_follow_ag_ui_stream(RemoteFollowStreamParams {
        config: params.config,
        session_id: params.session_id,
        run_id: params.run_id,
        thread_id: params.thread_id,
        subscription: params.subscription,
    })
    .await?;
    Ok(Some(stream))
}

/// Checks whether a remote worker holds the lease for this session.
async fn check_remote_lease(config: &Config, session_id: &str) -> Result<bool, AgUiError> {
    crate::ensure_frontend_nats_owner()
        .await
        .map_err(|err| AgUiError::Internal(format!("NATS unavailable: {err}")))?;
    let jetstream = config
        .nats_jetstream(LOCAL_CLUSTER_KEY)
        .await
        .map_err(|err| AgUiError::Internal(format!("JetStream unavailable: {err}")))?;
    session_has_active_lease(&jetstream, session_id)
        .await
        .map_err(|err| AgUiError::Internal(format!("Lease check failed: {err}")))
}

fn last_user_sequence(entries: &[(u64, SessionLogEntry)]) -> u64 {
    entries
        .iter()
        .rev()
        .find(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, .. }
                    if *role == harnx_core::message::MessageRole::User
            )
        })
        .map(|(seq, _)| *seq)
        .unwrap_or(0)
}

struct RemoteFollowStreamParams<'a> {
    config: &'a Config,
    session_id: &'a str,
    run_id: &'a str,
    thread_id: &'a str,
    subscription: &'a SubscribeResult,
}

async fn build_remote_follow_ag_ui_stream(
    params: RemoteFollowStreamParams<'_>,
) -> Result<AgUiEventStream, AgUiError> {
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

    build_remote_follow_event_stream(RemoteEventStreamParams {
        config: params.config,
        session_id: params.session_id,
        run_id: params.run_id,
        thread_id: params.thread_id,
        snapshot_frame,
    })
    .await
    .map_err(|err| AgUiError::Internal(format!("Remote follow failed: {err}")))
}

async fn build_remote_follow_event_stream(
    params: RemoteEventStreamParams<'_>,
) -> Result<AgUiEventStream> {
    let client = params.config.nats_client(LOCAL_CLUSTER_KEY).await?;
    let jetstream = params.config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
    let event_stream =
        SessionEventStream::attach(jetstream.clone(), client, params.session_id).await?;
    let started_frame = Bytes::from(frame_run_boundary_event(
        "RUN_STARTED",
        params.thread_id,
        params.run_id,
    ));

    // Compute through_seq from the single history snapshot (avoiding duplicate load).
    let through_seq = last_user_sequence(event_stream.history());

    if turn_ended(event_stream.history(), through_seq) {
        return Ok(completed_remote_stream(
            started_frame,
            params.snapshot_frame,
            params.thread_id,
            params.run_id,
        ));
    }

    Ok(build_live_follow_stream(LiveFollowParams {
        event_stream,
        jetstream,
        session_id: params.session_id.to_string(),
        started_frame,
        snapshot_frame: params.snapshot_frame,
        thread_id: params.thread_id.to_string(),
        run_id: params.run_id.to_string(),
        through_seq,
    }))
}

struct RemoteEventStreamParams<'a> {
    config: &'a Config,
    session_id: &'a str,
    run_id: &'a str,
    thread_id: &'a str,
    snapshot_frame: Option<Bytes>,
}

struct LiveFollowParams {
    event_stream: SessionEventStream,
    jetstream: JetstreamContext,
    session_id: String,
    started_frame: Bytes,
    snapshot_frame: Option<Bytes>,
    thread_id: String,
    run_id: String,
    through_seq: u64,
}

fn build_live_follow_stream(params: LiveFollowParams) -> AgUiEventStream {
    let finished = Arc::new(Notify::new());
    let (tx, rx) = tokio::sync::mpsc::channel(FRAME_CHANNEL_SIZE);

    spawn_follow_task(FollowTaskParams {
        event_stream: params.event_stream,
        jetstream: params.jetstream,
        session_id: params.session_id,
        tx,
        finished: finished.clone(),
        through_seq: params.through_seq,
    });

    let initial_frames = vec![params.started_frame]
        .into_iter()
        .chain(params.snapshot_frame);
    let event_frames = tokio_stream::wrappers::ReceiverStream::new(rx);
    let finished_stream = finished_event_stream(finished, params.thread_id, params.run_id);

    Box::pin(
        tokio_stream::iter(initial_frames)
            .chain(event_frames)
            .chain(finished_stream),
    )
}

fn completed_remote_stream(
    started_frame: Bytes,
    snapshot_frame: Option<Bytes>,
    thread_id: &str,
    run_id: &str,
) -> AgUiEventStream {
    let finished_frame = Bytes::from(frame_run_boundary_event("RUN_FINISHED", thread_id, run_id));
    let frames: Vec<Bytes> = vec![started_frame]
        .into_iter()
        .chain(snapshot_frame)
        .chain(std::iter::once(finished_frame))
        .collect();
    Box::pin(tokio_stream::iter(frames))
}

fn finished_event_stream(
    finished: Arc<Notify>,
    thread_id: String,
    run_id: String,
) -> impl Stream<Item = Bytes> + Send + Sync + 'static {
    tokio_stream::once(()).then(move |_| {
        let finished = Arc::clone(&finished);
        let thread_id = thread_id.clone();
        let run_id = run_id.clone();
        async move {
            finished.notified().await;
            Bytes::from(frame_run_boundary_event(
                "RUN_FINISHED",
                &thread_id,
                &run_id,
            ))
        }
    })
}

struct FollowTaskParams {
    event_stream: SessionEventStream,
    jetstream: JetstreamContext,
    session_id: String,
    tx: tokio::sync::mpsc::Sender<Bytes>,
    finished: Arc<Notify>,
    through_seq: u64,
}

/// RAII guard that ensures `finished.notify_one()` is called on drop.
///
/// Essential for robust stream termination: an early-return error path that
/// skips the notify would hang the client's SSE connection forever in a busy
/// state. Binding this guard as the first statement in the follow task ensures
/// every exit path (Ok, Err via `?`, or panic unwind) signals the `Notify` that
/// gates the terminal `RUN_FINISHED` frame.
struct NotifyOnDrop(Arc<Notify>);

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

fn spawn_follow_task(params: FollowTaskParams) {
    tokio::spawn(async move {
        if let Err(err) = remote_follow_task(params).await {
            log::warn!("Remote follow task error: {err:#}");
        }
    });
}

async fn remote_follow_task(mut params: FollowTaskParams) -> Result<()> {
    let _finished_guard = NotifyOnDrop(Arc::clone(&params.finished));
    let tx_for_close = params.tx.clone();
    let mut forwarder = AdvisoryForwarder::new(params.tx);
    let mut poller = RemoteTurnPoller::new(
        params.jetstream,
        params.session_id.clone(),
        params.through_seq,
    );
    let mut lease_poll_interval = tokio::time::interval(LEASE_POLL_INTERVAL);
    lease_poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            envelope = params.event_stream.next() => {
                if !forwarder.forward(&params.event_stream, envelope, &params.session_id) {
                    // Client dropped; return Ok - guard will notify.
                    return Ok(());
                }
            }
            _ = lease_poll_interval.tick() => {
                if poller.turn_finished(&mut params.event_stream).await? {
                    // Turn ended; return Ok - guard will notify.
                    return Ok(());
                }
            }
            _ = tx_for_close.closed() => return Ok(()),
        }
    }
}

struct AdvisoryForwarder {
    sink: AgUiSink,
    event_rx: UnboundedReceiver<Event>,
    tx: tokio::sync::mpsc::Sender<Bytes>,
}

impl AdvisoryForwarder {
    fn new(tx: tokio::sync::mpsc::Sender<Bytes>) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let message_id = ag_ui_core::types::ids::MessageId::random();
        Self {
            sink: AgUiSink::new_for_remote_follow(event_tx, message_id),
            event_rx,
            tx,
        }
    }

    fn forward(
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
        self.sink.emit(envelope.event);
        while let Ok(event) = self.event_rx.try_recv() {
            if let Ok(frame) = frame_event(&event) {
                match self.tx.try_send(Bytes::from(frame)) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        log::debug!(
                            "Dropping remote advisory frame for slow client on session {session_id}"
                        );
                    }
                    Err(TrySendError::Closed(_)) => return false,
                }
            }
        }
        true
    }
}

struct RemoteTurnPoller {
    jetstream: JetstreamContext,
    session_id: String,
    through_seq: u64,
    lease_absent_count: usize,
}

impl RemoteTurnPoller {
    fn new(jetstream: JetstreamContext, session_id: String, through_seq: u64) -> Self {
        Self {
            jetstream,
            session_id,
            through_seq,
            lease_absent_count: 0,
        }
    }

    async fn turn_finished(&mut self, event_stream: &mut SessionEventStream) -> Result<bool> {
        let lease_active = session_has_active_lease(&self.jetstream, &self.session_id).await?;
        let _history_updated = event_stream.refresh_history().await?;
        if turn_ended(event_stream.history(), self.through_seq) {
            log::debug!(
                "Turn end detected in durable history for session {}",
                self.session_id
            );
            return Ok(true);
        }

        if lease_active {
            self.lease_absent_count = 0;
            return Ok(false);
        }
        self.lease_absent_count += 1;
        if self.lease_absent_count < LEASE_ABSENT_THRESHOLD {
            return Ok(false);
        }

        log::warn!(
            "Lease absent for {} consecutive polls with no TurnEnd for session {}, forcing finish",
            self.lease_absent_count,
            self.session_id
        );
        Ok(true)
    }
}

fn turn_ended(history: &[(u64, SessionLogEntry)], through_seq: u64) -> bool {
    // Guard: through_seq==0 means no User message found; workers always append
    // User before running, so this state shouldn't occur. If it does, only
    // match TurnEnd entries with through_seq > 0 to avoid false positives.
    if through_seq == 0 {
        return false;
    }
    history.iter().rev().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::TurnEnd {
                through_seq: ended_through,
                ..
            } if *ended_through >= through_seq
        )
    })
}

impl AgUiSink {
    /// Creates a sink for the remote-follow path.
    ///
    /// Uses an unbounded event channel (advisory frames are loss-tolerant;
    /// the frame channel is bounded separately).
    pub(crate) fn new_for_remote_follow(
        tx: UnboundedSender<Event>,
        message_id: ag_ui_core::types::ids::MessageId,
    ) -> Self {
        Self::with_snapshot(tx, message_id, false, None)
    }
}
