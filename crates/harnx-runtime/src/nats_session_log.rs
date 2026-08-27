use anyhow::{bail, Context, Result};
use async_nats::jetstream::{
    self,
    message::PublishMessage,
    stream::{Config as StreamConfig, LastRawMessageErrorKind, RetentionPolicy},
};
use bytes::Bytes;
use harnx_core::{session::SessionLogEntry, session_log::SessionLog};
use std::time::Duration;
use tokio::runtime::{Builder, Handle};
use tokio::time::timeout;

const STREAM_NAME_PREFIX: &str = "SESSION_";
const DUPLICATE_WINDOW: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct NatsSessionLog {
    jetstream: jetstream::Context,
    session_id: String,
    stream_name: String,
    subject: String,
}

impl NatsSessionLog {
    pub fn new(jetstream: jetstream::Context, session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        Self {
            jetstream,
            stream_name: stream_name_for_session(&session_id),
            subject: subject_for_session(&session_id),
            session_id,
        }
    }

    pub async fn append_event_async(&self, entry: &SessionLogEntry) -> Result<u64> {
        self.append_event_with_message_id_async(entry, new_message_id())
            .await
    }

    pub async fn append_event_with_message_id_async(
        &self,
        entry: &SessionLogEntry,
        message_id: impl Into<String>,
    ) -> Result<u64> {
        self.append_event_with_publish_message_async(
            entry,
            PublishMessage::build().message_id(message_id.into()),
        )
        .await
    }

    pub async fn append_event_with_expected_last_sequence_async(
        &self,
        entry: &SessionLogEntry,
        expected_last_sequence: u64,
    ) -> Result<u64> {
        self.append_event_with_publish_message_async(
            entry,
            PublishMessage::build()
                .message_id(new_message_id())
                .expected_last_sequence(expected_last_sequence),
        )
        .await
    }

    async fn append_event_with_publish_message_async(
        &self,
        entry: &SessionLogEntry,
        publish_message: PublishMessage,
    ) -> Result<u64> {
        self.ensure_stream().await?;
        let payload = serialize_entry(entry)?;
        let ack = self
            .jetstream
            .send_publish(
                self.subject.clone(),
                publish_message.payload(Bytes::from(payload)),
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to publish session log entry for session '{}'",
                    self.session_id
                )
            })?
            .await
            .with_context(|| {
                format!(
                    "JetStream did not ack session log entry for session '{}'",
                    self.session_id
                )
            })?;
        Ok(ack.sequence)
    }

    pub async fn load_events_async(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        self.ensure_stream().await?;
        self.read_all_from(1).await
    }

    /// Load durable entries appended after `seq`, retaining their stream
    /// sequence numbers so an attached client can advance its replay cursor.
    pub async fn load_events_after_async(&self, seq: u64) -> Result<Vec<(u64, SessionLogEntry)>> {
        let Some(start_sequence) = seq.checked_add(1) else {
            return Ok(Vec::new());
        };
        self.read_all_from(start_sequence).await
    }

    /// Like [`load_events_async`], but first waits (bounded) until the stream's
    /// `last_sequence` reaches `min_seq`, providing read-your-writes consistency.
    ///
    /// A publish ack returns the durable sequence, but a subsequent `stream.info()`
    /// round-trip can momentarily report a lower `last_sequence`. A worker that
    /// re-reads the log immediately after persisting its own turn output (e.g. the
    /// end-of-turn drain re-read) must observe its own writes, otherwise it
    /// re-folds already-answered messages. Pass the max sequence the worker has
    /// appended to ensure the read reflects it. `min_seq == 0` is a plain load.
    pub async fn load_events_at_least_async(
        &self,
        min_seq: u64,
    ) -> Result<Vec<(u64, SessionLogEntry)>> {
        let mut stream = self.ensure_stream().await?;
        if min_seq > 0 {
            let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
            loop {
                let last_sequence = stream
                    .info()
                    .await
                    .map(|info| info.state.last_sequence)
                    .unwrap_or(0);
                if last_sequence >= min_seq || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        self.read_all_from(1).await
    }

    pub async fn replay_from_async(&self, seq: u64) -> Result<Vec<SessionLogEntry>> {
        self.load_events_after_async(seq)
            .await
            .map(|entries| entries.into_iter().map(|(_, entry)| entry).collect())
    }

    pub async fn load_events_latest_async(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        let mut stream = self.ensure_stream().await?;
        let latest = match stream.get_last_raw_message_by_subject(&self.subject).await {
            Ok(raw) => raw,
            Err(err) if matches!(err.kind(), LastRawMessageErrorKind::NoMessageFound) => {
                return Ok(Vec::new());
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "Failed to get latest JetStream session log entry for session '{}' subject '{}'",
                        self.session_id, self.subject
                    )
                });
            }
        };
        let first_sequence = stream
            .info()
            .await
            .with_context(|| {
                format!(
                    "Failed to inspect JetStream log stream '{}' for session '{}'",
                    self.stream_name, self.session_id
                )
            })?
            .state
            .first_sequence;
        let start = first_sequence.max(1);

        self.read_range(&stream, start, latest.sequence).await
    }

    async fn ensure_stream(&self) -> Result<jetstream::stream::Stream> {
        self.jetstream
            .get_or_create_stream(StreamConfig {
                name: self.stream_name.clone(),
                subjects: vec![self.subject.clone()],
                retention: RetentionPolicy::Limits,
                duplicate_window: DUPLICATE_WINDOW,
                ..Default::default()
            })
            .await
            .with_context(|| {
                format!(
                    "Failed to create or open JetStream log stream '{}' for session '{}'",
                    self.stream_name, self.session_id
                )
            })
    }

    /// Read all persisted entries with stream sequence `>= start_sequence`.
    ///
    /// Uses JetStream's direct per-sequence `get_raw_message` lookup rather than
    /// a consumer. For a bounded historical read this is deterministic and
    /// terminates cleanly: a consumer's message stream is a live subscription
    /// that blocks waiting for new messages, and the pull-consumer `fetch` path
    /// proved flaky here (partial/short batches under a current-thread Tokio
    /// runtime). Direct get works regardless of runtime flavor and never blocks
    /// on future messages. Each lookup is bounded by `READ_TIMEOUT`.
    async fn read_all_from(&self, start_sequence: u64) -> Result<Vec<(u64, SessionLogEntry)>> {
        let mut stream = self.ensure_stream().await?;
        let stream_info = stream.info().await.with_context(|| {
            format!(
                "Failed to inspect JetStream log stream '{}' for session '{}'",
                self.stream_name, self.session_id
            )
        })?;
        let first_sequence = stream_info.state.first_sequence;
        let last_sequence = stream_info.state.last_sequence;
        let message_count = stream_info.state.messages;

        if message_count == 0 || last_sequence < start_sequence {
            return Ok(Vec::new());
        }

        self.read_range(
            &stream,
            start_sequence.max(first_sequence).max(1),
            last_sequence,
        )
        .await
    }

    async fn read_range(
        &self,
        stream: &jetstream::stream::Stream,
        start_sequence: u64,
        last_sequence: u64,
    ) -> Result<Vec<(u64, SessionLogEntry)>> {
        if last_sequence < start_sequence {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        for seq in start_sequence..=last_sequence {
            let raw = match timeout(READ_TIMEOUT, stream.get_raw_message(seq)).await {
                Ok(Ok(raw)) => raw,
                // Sequence may have been removed (retention/limits) leaving a gap;
                // skip missing sequences rather than failing the whole read.
                Ok(Err(_)) => continue,
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "Timed out after {:?} reading JetStream session log for session '{}' at stream sequence {}",
                        READ_TIMEOUT,
                        self.session_id,
                        seq
                    ));
                }
            };
            let entry = deserialize_entry(&raw.payload).with_context(|| {
                format!(
                    "Failed to deserialize session log entry for session '{}' at stream sequence {}",
                    self.session_id, raw.sequence
                )
            })?;
            entries.push((raw.sequence, entry));
        }
        Ok(entries)
    }
}

impl SessionLog for NatsSessionLog {
    fn append_event(&mut self, entry: &SessionLogEntry) -> Result<u64> {
        block_on_session_log_future(self.append_event_async(entry))?
    }

    fn load_events(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        block_on_session_log_future(self.load_events_async())?
    }

    fn replay_from(&self, seq: u64) -> Result<Vec<SessionLogEntry>> {
        block_on_session_log_future(self.replay_from_async(seq))?
    }
}

pub fn serialize_entry(entry: &SessionLogEntry) -> Result<String> {
    serde_yaml::to_string(entry).context("Failed to serialize SessionLogEntry")
}

pub fn deserialize_entry(payload: &[u8]) -> Result<SessionLogEntry> {
    serde_yaml::from_slice(payload).context("Failed to deserialize SessionLogEntry")
}

pub fn load_session_from_entries(
    entries: &[(u64, SessionLogEntry)],
    name: &str,
) -> Result<harnx_core::session::Session> {
    let raw_entries: Vec<(usize, SessionLogEntry)> = entries
        .iter()
        .map(|(seq, entry)| {
            usize::try_from(*seq)
                .map(|seq| (seq, entry.clone()))
                .context("session log sequence does not fit into usize")
        })
        .collect::<Result<_>>()?;
    crate::config::session::replay_log_entries_for_external(&raw_entries, name)
}

pub fn load_session_from_entries_with_metadata(
    entries: &[(u64, SessionLogEntry)],
    name: &str,
    session: harnx_core::session::Session,
) -> Result<harnx_core::session::Session> {
    let raw_entries: Vec<(usize, SessionLogEntry)> = entries
        .iter()
        .map(|(seq, entry)| {
            usize::try_from(*seq)
                .map(|seq| (seq, entry.clone()))
                .context("session log sequence does not fit into usize")
        })
        .collect::<Result<_>>()?;
    crate::config::session::replay_nats_entries_into_session(&raw_entries, name, session)
}

pub fn load_session_from_yaml(content: &str, name: &str) -> Result<harnx_core::session::Session> {
    let raw_entries = crate::config::session::collect_raw_log_entries(content, name)?;
    crate::config::session::replay_log_entries_for_external(&raw_entries, name)
}

pub fn subject_for_session(session_id: &str) -> String {
    format!("sessions.{session_id}.log")
}

pub fn stream_name_for_session(session_id: &str) -> String {
    let mut name = String::with_capacity(STREAM_NAME_PREFIX.len() + session_id.len());
    name.push_str(STREAM_NAME_PREFIX);
    for ch in session_id.chars() {
        name.push(sanitize_stream_name_char(ch));
    }
    name
}

fn sanitize_stream_name_char(ch: char) -> char {
    if is_valid_stream_name_char(ch) {
        ch.to_ascii_uppercase()
    } else {
        '_'
    }
}

fn is_valid_stream_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

/// Unique idempotency key for a single append.
///
/// Each logical append gets a fresh UUID so that two distinct entries with
/// identical serialized content (e.g. two identical user messages, or two
/// duplicate log entries with the same text are BOTH stored — a content-derived
/// id would make JetStream's `duplicate_window` silently drop the second.
/// The id still protects against an accidental double-publish of the *same*
/// `send_publish` call within the dedup window.
fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn block_on_session_log_future<F>(future: F) -> Result<F::Output>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    if Handle::try_current().is_ok() {
        bail!(
            "NatsSessionLog: use append_event_async/load_events_async/replay_from_async from within a Tokio runtime; sync SessionLog methods are only supported from non-async callers"
        );
    }

    Ok(Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to build temporary Tokio runtime for NatsSessionLog")?
        .block_on(future))
}
