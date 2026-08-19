//! P4.1: Live event fan-out from worker to clients.
//!
//! The worker forwards its live `AgentEvent` stream (text chunks, tool progress,
//! status) to a non-durable NATS fan-out subject `sessions.{session_id}.events`.
//! Multiple clients can attach to one session: each replays the durable log tail
//! for history, then subscribes to live events — gap-free and dup-free.
//!
//! ## Durable vs advisory contract
//!
//! - DURABLE (JetStream session log, authoritative): UserMessage, AssistantMessage,
//!   ToolCalls, ToolResults, TurnEnd, Error, Cancel. Reconstructable via
//!   NatsSessionLog + reconstruct_state.
//! - ADVISORY (fan-out `sessions.{id}.events` only, non-durable, lossy-OK): streaming token
//!   deltas (ModelEvent::MessageChunk/ThoughtChunk), thinking/status indicators,
//!   tool-progress chunks. NEVER authoritative; safe to miss.
//! - Each advisory message carries `after_seq: u64` = the session-log stream's last_seq
//!   at emit time, enabling gap-free client attach.

use anyhow::Result;
use async_nats::jetstream;
use harnx_core::event::{AgentEvent, AgentEventSink};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Advisory subject pattern for a session's live events.
///
/// Format: `sessions.{session_id}.events`
pub fn events_subject(session_id: &str) -> String {
    format!("sessions.{session_id}.events")
}

/// Envelope for advisory events published to the fan-out subject.
///
/// Each advisory message carries `after_seq` = the session-log stream's current
/// `last_seq` at emit time. Clients use this for dedup/ordering:
/// - Durable entries are applied by stream seq (monotonic, idempotent)
/// - An advisory envelope is rendered only if `after_seq >= client's last-applied durable seq`
///
/// This ensures:
/// - No gap: durable replay complete to last_seq, then continue with live events
/// - No dup: seq idempotent, advisory filtered by after_seq
/// - On failover, advisory deltas may drop mid-turn; final state reconciles from durable log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisoryEnvelope {
    /// The session-log stream sequence this advisory follows.
    /// Clients should only render this event if they have applied all durable
    /// entries up to and including this sequence.
    pub after_seq: u64,
    /// The actual agent event (Model chunk, tool progress, status, etc.)
    pub event: AgentEvent,
}

impl AdvisoryEnvelope {
    /// Create a new advisory envelope.
    pub fn new(after_seq: u64, event: AgentEvent) -> Self {
        Self { after_seq, event }
    }

    /// Serialize to JSON bytes for NATS publish.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Deserialize from JSON bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }
}

/// NATS event sink that publishes AgentEvents to the advisory fan-out subject.
///
/// Implements `AgentEventSink` and wraps the worker's shared NATS client.
/// Each event is wrapped in an `AdvisoryEnvelope` with the current session-log
/// stream `last_seq` as `after_seq` for client-side dedup.
#[derive(Clone)]
pub struct NatsEventSink {
    /// NATS client for publishing (core NATS, non-durable).
    client: async_nats::Client,
    /// Cached fan-out subject (`sessions.{id}.events`).
    subject: String,
    /// Cached durable-log sequence this sink stamps on advisories as
    /// `after_seq`. Maintained WITHOUT a per-event JetStream round-trip:
    /// seeded at construction and advanced by the worker via
    /// [`NatsEventSink::note_durable_seq`] as durable entries are appended.
    /// Stale-low values are safe (a caught-up client just drops that advisory;
    /// advisory delivery is lossy by contract).
    after_seq: Arc<AtomicU64>,
    /// Ordered hand-off to the single publisher task. `emit` stamps and enqueues
    /// synchronously; one task drains this FIFO so advisories reach NATS in
    /// emission order. Previously `emit` spawned a task per event, which let
    /// concurrent publishes finish in any order — a warning and the notice that
    /// followed it could arrive swapped, and no consumer could recover the
    /// intended sequence.
    publisher: tokio::sync::mpsc::UnboundedSender<AdvisoryEnvelope>,
}

impl NatsEventSink {
    /// Create a new NATS event sink, seeding `after_seq` from the session
    /// log stream's current `last_sequence` (a single query, NOT per-event).
    ///
    /// # Arguments
    /// * `client` - Shared NATS client for core publish (non-durable)
    /// * `jetstream` - JetStream context, used ONCE to seed the seq
    /// * `session_id` - Session ID for subject routing
    pub async fn new(
        client: async_nats::Client,
        jetstream: jetstream::Context,
        session_id: impl Into<String>,
    ) -> Self {
        let session_id = session_id.into();
        let stream_name = crate::nats_session_log::stream_name_for_session(&session_id);
        let seed = match jetstream.get_stream(&stream_name).await {
            Ok(mut stream) => stream
                .info()
                .await
                .map(|i| i.state.last_sequence)
                .unwrap_or(0),
            Err(_) => 0,
        };
        let subject = events_subject(&session_id);
        let (publisher, mut rx) = tokio::sync::mpsc::unbounded_channel::<AdvisoryEnvelope>();
        let publisher_client = client.clone();
        let publisher_subject = subject.clone();
        // Ends when every clone of this sink is dropped and the channel closes.
        tokio::spawn(async move {
            while let Some(envelope) = rx.recv().await {
                publish_envelope(&publisher_client, &publisher_subject, envelope).await;
            }
        });
        Self {
            client,
            subject,
            after_seq: Arc::new(AtomicU64::new(seed)),
            publisher,
        }
    }

    /// Advance the cached `after_seq` to `seq` (monotonic). The worker calls
    /// this after appending a durable session-log entry so subsequent
    /// advisories carry the correct ordering hint without a JetStream query.
    pub fn note_durable_seq(&self, seq: u64) {
        self.after_seq.fetch_max(seq, Ordering::Relaxed);
    }

    /// Handle to the shared `after_seq` cell, so the worker's append path can
    /// advance it as durable entries land.
    pub fn after_seq_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.after_seq)
    }

    /// Publish an event to the advisory fan-out subject. Best-effort core NATS
    /// (non-durable); failures are logged, never propagated. No JetStream
    /// round-trip — `after_seq` comes from the cached atomic.
    pub async fn publish_event(&self, event: AgentEvent) {
        let after_seq = self.after_seq.load(Ordering::Relaxed);
        publish_envelope(
            &self.client,
            &self.subject,
            AdvisoryEnvelope::new(after_seq, event),
        )
        .await;
    }
}

/// Publish one advisory. Best-effort core NATS (non-durable); failures are
/// logged, never propagated.
async fn publish_envelope(client: &async_nats::Client, subject: &str, envelope: AdvisoryEnvelope) {
    match envelope.to_bytes() {
        Ok(payload) => {
            if let Err(e) = client.publish(subject.to_string(), payload.into()).await {
                log::debug!("advisory event publish failed (lossy-OK): {e}");
            }
        }
        Err(e) => log::debug!("failed to serialize advisory event: {e}"),
    }
}

impl AgentEventSink for NatsEventSink {
    fn emit(&self, event: AgentEvent) {
        // Stamp `after_seq` and enqueue synchronously, so both the ordering hint
        // and the queue position reflect the order events were emitted in. The
        // publisher task then sends them one at a time.
        //
        // This used to spawn a task per event, which never blocked the agent loop
        // but also gave up ordering: N detached publishes race, so a notice could
        // overtake the one emitted before it. Enqueuing is just as non-blocking
        // and needs no runtime handle. Delivery stays lossy by contract — a send
        // failure means the publisher task is gone, and is dropped as before.
        let after_seq = self.after_seq.load(Ordering::Relaxed);
        let _ = self.publisher.send(AdvisoryEnvelope::new(after_seq, event));
    }
}

// ---------------------------------------------------------------------------
// Client-side attach helper
// ---------------------------------------------------------------------------

/// Client-side helper for attaching to a session's event stream.
///
/// Provides gap-free, dup-free event delivery:
/// 1. Opens the durable NatsSessionLog, records current stream last_seq
/// 2. Replays log [first..=last_seq] to build history
/// 3. Subscribes to `sessions.{id}.events`
/// 4. Dedup rule: durable entries applied by seq; advisory only if after_seq >= last-applied
///
/// # Example
///
/// ```ignore
/// use harnx_runtime::nats_event_sink::SessionEventStream;
///
/// async fn attach_and_consume(jetstream: jetstream::Context, client: async_nats::Client, session_id: &str) {
///     let stream = SessionEventStream::attach(jetstream, client, session_id).await.unwrap();
///     
///     // First, process durable history
///     for (seq, entry) in stream.history() {
///         // Apply entry to local state
///     }
///     
///     // Then, consume live events
///     while let Some(envelope) = stream.next().await {
///         if envelope.after_seq >= stream.last_applied_seq() {
///             // Render the advisory event
///         }
///     }
/// }
/// ```
pub struct SessionEventStream {
    /// History replayed from durable log (seq, entry pairs).
    history: Vec<(u64, harnx_core::session::SessionLogEntry)>,
    /// Last sequence number from durable history (for dedup).
    last_durable_seq: u64,
    /// Subscription to the advisory events subject.
    subscriber: async_nats::Subscriber,
}

impl SessionEventStream {
    /// Attach to a session's event stream.
    ///
    /// 1. Opens the durable session log stream, replays history
    /// 2. Subscribes to the advisory events subject
    /// 3. Returns history and live event stream
    ///
    /// # Arguments
    /// * `jetstream` - JetStream context for durable log access
    /// * `client` - NATS client for advisory subscription
    /// * `session_id` - Session to attach to
    pub async fn attach(
        jetstream: jetstream::Context,
        client: async_nats::Client,
        session_id: &str,
    ) -> Result<Self> {
        // 1. Subscribe to advisory events FIRST, then load durable history.
        //    Ordering matters: any advisory emitted while we replay the log
        //    would be lost if we subscribed afterwards. Subscribing first means
        //    such advisories are buffered by the subscription; the `after_seq`
        //    dedup rule then drops the ones already covered by the history we
        //    load next, and renders the genuinely-newer ones — no gap, no dup.
        let subject = events_subject(session_id);
        let subscriber = client
            .subscribe(subject)
            .await
            .map_err(|e| anyhow::anyhow!("failed to subscribe to events subject: {e}"))?;

        // 2. Load durable history (after subscribing).
        let log = crate::nats_session_log::NatsSessionLog::new(jetstream.clone(), session_id);
        let history = log.load_events_async().await?;
        let last_durable_seq = history.last().map(|(seq, _)| *seq).unwrap_or(0);

        Ok(Self {
            history,
            last_durable_seq,
            subscriber,
        })
    }

    /// Get the durable history (replayed entries).
    pub fn history(&self) -> &[(u64, harnx_core::session::SessionLogEntry)] {
        &self.history
    }

    /// Get the last applied durable sequence number.
    pub fn last_applied_seq(&self) -> u64 {
        self.last_durable_seq
    }

    /// Receive the next advisory envelope.
    ///
    /// Returns `None` when the subscription is closed.
    ///
    /// Client dedup rule: render the advisory only if `should_render` is true
    /// (i.e. `after_seq >= last_applied_seq()`).
    pub async fn next(&mut self) -> Option<AdvisoryEnvelope> {
        use futures_util::StreamExt;

        loop {
            let msg = self.subscriber.next().await?;
            match AdvisoryEnvelope::from_bytes(&msg.payload) {
                Ok(envelope) => return Some(envelope),
                Err(error) => {
                    log::warn!(
                        "skipping malformed NATS advisory payload for session event stream: {error:#}"
                    );
                }
            }
        }
    }

    /// Check if an advisory envelope should be rendered.
    ///
    /// Dedup rule: render only if the advisory's `after_seq` is `>=` the last
    /// durable seq the client has applied. `>=` (not `>`) is required so a
    /// freshly-attached client (history replayed to seq N) still renders the
    /// LIVE advisories of the in-flight turn — those carry `after_seq = N`
    /// because they *follow* durable entry N (the next durable entry,
    /// e.g. the final AssistantMessage, is not yet written). A stale advisory
    /// from before the client's position (`after_seq < last_durable_seq`) is
    /// dropped. The authoritative final state always comes from the durable
    /// log; advisories are lossy previews.
    pub fn should_render(&self, envelope: &AdvisoryEnvelope) -> bool {
        envelope.after_seq >= self.last_durable_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::{ModelEvent, NoticeEvent};

    #[test]
    fn advisory_envelope_serializes() {
        let event = AgentEvent::Notice(NoticeEvent::Info("hello".into()));
        let envelope = AdvisoryEnvelope::new(42, event);
        let bytes = envelope.to_bytes().unwrap();
        assert!(!bytes.is_empty());

        let back = AdvisoryEnvelope::from_bytes(&bytes).unwrap();
        assert_eq!(back.after_seq, 42);
    }

    #[test]
    fn advisory_envelope_wraps_model_chunk() {
        let event = AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![harnx_core::event::ContentBlock::Text("Hello world".into())],
        });
        let envelope = AdvisoryEnvelope::new(100, event);
        let bytes = envelope.to_bytes().unwrap();
        let back = AdvisoryEnvelope::from_bytes(&bytes).unwrap();
        assert_eq!(back.after_seq, 100);
    }

    #[test]
    fn should_render_dedup_rule() {
        // Mirrors SessionEventStream::should_render. last_durable_seq = 10:
        // - after_seq = 15 -> render (newer advisory)
        // - after_seq = 10 -> RENDER: live advisories of the in-flight turn
        //   carry the durable seq they FOLLOW (10); a client caught up to 10
        //   must still see them as live preview.
        // - after_seq = 5  -> DROP (stale, predates client position)
        fn render(after_seq: u64, last_durable_seq: u64) -> bool {
            after_seq >= last_durable_seq
        }
        assert!(render(15, 10), "newer advisory must render");
        assert!(
            render(10, 10),
            "live advisory following the client's last durable seq must render"
        );
        assert!(!render(5, 10), "stale advisory must not render");

        // Sanity: envelopes carry the after_seq through serde.
        let event = AgentEvent::Notice(NoticeEvent::Info("test".into()));
        let env = AdvisoryEnvelope::new(15, event);
        let back = AdvisoryEnvelope::from_bytes(&env.to_bytes().unwrap()).unwrap();
        assert_eq!(back.after_seq, 15);
    }
}
