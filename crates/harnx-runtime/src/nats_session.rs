//! Session driver for NATS agents (agent@cluster).
//!
//! The client posts the user message + control commands to NATS and renders
//! events from the fan-out — it does not run `run_agent_loop` itself. A
//! separate `harnx-worker --cluster <key>` daemon executes the turn.
//!
//! ## Architecture
//!
//! ```text
//! [Client]                                   [Worker Daemon]
//!    |                                            |
//!    |--1. Append user message (durable)--------->|
//!    |                                            |
//!    |--2. SessionEventStream::attach()           |
//!    |    (subscribe-first, then history)         |
//!    |                                            |
//!    |--3. publish_session_activate()------------>|-- wake up
//!    |                                            |-- claim lease
//!    |                                            |-- run_agent_loop
//!    |<----------- advisory events ---------------|
//!    |                                            |-- append final state
//!    |--4. Observe durable TurnEnd ---------------|
//!    |                                            |
//!    |--5. Return final response                  |
//! ```
//!
//! ## Turn completion detection
//!
//! New workers append a durable `TurnEnd` after the complete model/tool/hook
//! loop, including the highest user-message sequence it consumed. That marker
//! is the authoritative success boundary. For compatibility with older
//! workers, an assistant row plus either a parent `Turn::Ended` advisory or a
//! confirmed worker-lease release is accepted as a fallback. Durable errors
//! and cancellation remain terminal barriers.

use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::abort::wait_abort_signal;
use harnx_core::event::{
    AgentEvent, AgentEventSink, ModelEvent, SessionEvent, TurnEvent, UserEvent,
};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::{
    active_context_window, reconstruct_state_from_nats, ActiveContextWindow,
};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use crate::nats_event_sink::SessionEventStream;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
use crate::nats_worker::{
    new_remote_session_id, publish_control_command, publish_session_activate,
    publish_targeted_session_activate, request_control_command, ControlCommand, LocalWorkerTarget,
    SessionActivate, SessionActivationRoute,
};
use crate::utils::AbortSignal;

/// Generate a client-side message ID (UUID v4).
pub(crate) fn new_client_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// How often the client re-reads the session lease. Slower than the durable
/// completion check because a worker's death is not latency-sensitive and each
/// read is a NATS round trip on every in-flight turn.
const ORPHAN_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Consecutive lease reads with no holder before a turn is declared orphaned.
/// Two reads at the interval above leave ~4s of slack for a worker's lease
/// release to race the barrier it just wrote.
const ORPHAN_MISSING_CHECKS: u32 = 2;
const CONTROL_ACK_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const CONTROL_ACK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
const CANCEL_RECOVERY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(crate::nats_lease::DEFAULT_LEASE_TTL.as_secs() + 10);

/// Lease-backed detector for a session whose worker disappeared without
/// writing a durable terminal entry.
///
/// The worker writes its assistant message (or its `Error` entry) before
/// releasing the session lease, so "lease gone, nothing in the log" means the
/// worker died — a panic, a kill, or a lost connection. The first observed
/// lease arms the detector. A session that is merely queued and has never been
/// claimed therefore remains pending, while a lease that later expires is
/// reported as orphaned after a short confirmation window.
pub struct SessionLeaseWatchdog {
    lease_config: crate::nats_lease::NatsLeaseConfig,
    /// Opened once and reused: `get_key_value` costs a `stream_info` round trip
    /// that the per-poll `get` does not.
    bucket: Option<async_nats::jetstream::kv::Store>,
    next_check: tokio::time::Instant,
    saw_lease: bool,
    missing_checks: u32,
}

impl SessionLeaseWatchdog {
    pub fn new() -> Self {
        Self {
            lease_config: crate::nats_lease::NatsLeaseConfig::default(),
            bucket: None,
            next_check: tokio::time::Instant::now() + ORPHAN_CHECK_INTERVAL,
            saw_lease: false,
            missing_checks: 0,
        }
    }

    /// The failure message to end the turn with, or `None` to keep waiting.
    pub async fn check(
        &mut self,
        jetstream: &jetstream::Context,
        session_id: &str,
    ) -> Option<String> {
        let now = tokio::time::Instant::now();
        if now < self.next_check {
            return None;
        }
        self.next_check = now + ORPHAN_CHECK_INTERVAL;

        if self.bucket.is_none() {
            self.bucket = crate::nats_lease::open_lease_bucket(jetstream, &self.lease_config).await;
        }
        // No bucket means no worker has ever leased on this cluster; that is
        // indistinguishable from "not started yet", so keep waiting.
        let bucket = self.bucket.as_ref()?;

        let holder = match crate::nats_lease::lease_holder_in(
            bucket,
            &self.lease_config,
            session_id,
        )
        .await
        {
            Ok(holder) => holder,
            Err(error) => {
                // An unreadable lease says nothing about the worker.
                log::debug!("nats session: lease liveness check failed: {error:#}");
                self.missing_checks = 0;
                return None;
            }
        };

        let Some(holder) = holder else {
            if !self.saw_lease {
                return None;
            }
            self.missing_checks += 1;
            if self.missing_checks < ORPHAN_MISSING_CHECKS {
                return None;
            }
            return Some(
                "The worker handling this session stopped without answering. \
                 Check the worker log for the underlying failure."
                    .to_string(),
            );
        };

        log::trace!("nats session: {session_id} held by {}", holder.worker_id);
        self.saw_lease = true;
        self.missing_checks = 0;
        None
    }
}

impl Default for SessionLeaseWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for a NATS session.
#[derive(Clone)]
pub struct NatsSessionConfig {
    /// Cluster key (from AgentRef::Remote { cluster, ... }).
    pub cluster: String,
    /// Immutable identity and initial persisted values for a named or inline
    /// session. Existing sessions validate only the immutable agent source.
    pub initializer: SessionInitializer,
    /// Existing session ID to resume/attach (None = new session).
    pub session_id: Option<String>,
    /// Where turn activations are dispatched. History access remains tied only
    /// to `cluster` and `session_id`.
    pub activation_route: SessionActivationRoute,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestedSeqStatus {
    Pending,
    Covered,
}

async fn ensure_session_metadata(
    store: &SessionMetadataStore,
    jetstream: &jetstream::Context,
    session_id: &str,
    initializer: &SessionInitializer,
) -> Result<()> {
    if let Some(record) = store.get(session_id).await? {
        record.metadata.validate_initializer(initializer)?;
        store.ensure_reserved_activity(session_id).await?;
        return Ok(());
    }

    let log = NatsSessionLog::new(jetstream.clone(), session_id.to_string());
    let existing_entries = log
        .load_events_async()
        .await
        .context("failed to inspect transcript before creating session metadata")?;
    anyhow::ensure!(
        existing_entries.is_empty(),
        "session '{session_id}' has transcript entries but no canonical metadata; legacy or inconsistent sessions are not supported"
    );

    let metadata = SessionMetadata::new(session_id, initializer.clone());
    if store.create(&metadata).await?.is_some() {
        return Ok(());
    }

    // Another client won the create race. Its immutable identity must agree
    // with ours before either client is allowed to append a first message.
    let winner = store.get(session_id).await?.with_context(|| {
        format!("session metadata creation race for '{session_id}' had no winner")
    })?;
    winner.metadata.validate_initializer(initializer)?;
    store.ensure_reserved_activity(session_id).await
}

/// Determine whether a durable request still needs worker execution.
///
/// This is shared by the client completion loop and targeted workers so turn
/// completion, failure, cancellation, and retraction semantics cannot drift.
pub(crate) fn requested_seq_status(
    entries: &[(u64, SessionLogEntry)],
    requested_seq: u64,
) -> Result<RequestedSeqStatus> {
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    Ok(requested_seq_status_with_effective(
        entries,
        &effective,
        requested_seq,
    ))
}

fn requested_seq_status_with_effective(
    entries: &[(u64, SessionLogEntry)],
    effective: &[(u64, SessionLogEntry)],
    requested_seq: u64,
) -> RequestedSeqStatus {
    if entries.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::TurnEnd { through_seq, .. } if *through_seq >= requested_seq
        )
    }) || entries.iter().any(|(seq, entry)| {
        *seq > requested_seq
            && matches!(
                entry,
                SessionLogEntry::Error { .. } | SessionLogEntry::Cancel { .. }
            )
    }) {
        return RequestedSeqStatus::Covered;
    }

    let requested_still_exists = effective.iter().any(|(seq, _)| *seq == requested_seq);
    if requested_still_exists {
        return RequestedSeqStatus::Pending;
    }

    let reconstructed = reconstruct_state_from_nats(entries);
    if reconstructed.next_turn_messages.is_empty() && reconstructed.resumable_ctx.is_none() {
        RequestedSeqStatus::Covered
    } else {
        RequestedSeqStatus::Pending
    }
}

/// Session driver.
///
/// Orchestrates the workflow: append user message, activate, stream events,
/// detect completion.
pub struct NatsSession {
    config: NatsSessionConfig,
    session_id: String,
    jetstream: jetstream::Context,
    client: async_nats::Client,
    abort_signal: AbortSignal,
    metadata_store: SessionMetadataStore,
    attachment_replicas: usize,
}

/// Result of durably queueing one prompt for worker execution.
pub(crate) struct EnqueuedPrompt {
    pub session_id: String,
    pub user_msg_seq: u64,
}

/// Result of durably appending text that may still need worker activation.
pub struct DurableTextEnqueue {
    user_msg_seq: u64,
    activation_error: Option<anyhow::Error>,
}

impl DurableTextEnqueue {
    /// Sequence assigned to the durable user message.
    pub fn user_msg_seq(&self) -> u64 {
        self.user_msg_seq
    }

    /// Publication failure that left the durable message pending activation.
    pub fn activation_error(&self) -> Option<&anyhow::Error> {
        self.activation_error.as_ref()
    }

    /// Convert the outcome into the durable sequence or publication error.
    pub fn into_activation_result(self) -> Result<u64> {
        if let Some(error) = self.activation_error {
            return Err(error);
        }
        Ok(self.user_msg_seq)
    }
}

struct AppendedPrompt {
    user_msg_id: String,
    user_msg_seq: u64,
}

impl NatsSession {
    /// Create a new NATS session.
    ///
    /// Connects to the NATS cluster and generates/reuses a session ID.
    ///
    /// This function takes the NATS connection components directly to avoid
    /// Send issues with GlobalConfig's parking_lot lock guard across await points.
    pub async fn new(
        config: NatsSessionConfig,
        client: async_nats::Client,
        jetstream: jetstream::Context,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        // Resolve session ID (new or existing)
        let session_id = config
            .session_id
            .clone()
            .unwrap_or_else(new_remote_session_id);

        let metadata_store = SessionMetadataStore::ensure(&jetstream, 1)
            .await
            .context("failed to open canonical session metadata store")?;
        ensure_session_metadata(
            &metadata_store,
            &jetstream,
            &session_id,
            &config.initializer,
        )
        .await?;

        Ok(Self {
            config,
            session_id,
            jetstream,
            client,
            abort_signal,
            metadata_store,
            attachment_replicas: 1,
        })
    }

    /// Convenience constructor that builds NATS connections from GlobalConfig.
    pub async fn from_global_config(
        config: NatsSessionConfig,
        global_config: &crate::config::GlobalConfig,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let cluster = config.cluster.clone();
        let config_snapshot = global_config.read().clone();
        let attachment_replicas = config_snapshot
            .resolve_nats_server(&cluster)
            .await?
            .resolved_replicas();
        let client = config_snapshot
            .nats_client(&cluster)
            .await
            .context("failed to connect to NATS cluster")?;
        let jetstream = async_nats::jetstream::new(client.clone());

        let mut nats_session = Self::new(config, client, jetstream, abort_signal).await?;
        nats_session.attachment_replicas = attachment_replicas;

        // Front-end dot commands mutate the active Config session synchronously.
        // Give that in-memory session a metadata-capable sink so `.model`,
        // `.set`, and title changes commit through CAS before local state moves.
        let backend = crate::nats_worker::NatsSessionLogBackend::new(
            nats_session.jetstream.clone(),
            nats_session.session_id.clone(),
        )
        .with_metadata_store(Some(nats_session.metadata_store.clone()));
        let sink = Arc::new(backend) as Arc<dyn crate::config::session::SessionAppendSink>;
        if let Some(active_session) = global_config.write().session.as_mut() {
            if active_session.id() == nats_session.session_id {
                active_session.runtime = Some(Arc::new(sink));
            }
        }

        Ok(nats_session)
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn jetstream(&self) -> &jetstream::Context {
        &self.jetstream
    }

    pub fn metadata_store(&self) -> &SessionMetadataStore {
        &self.metadata_store
    }

    async fn append_user_content(&self, content: MessageContent) -> Result<AppendedPrompt> {
        let user_msg_id = new_client_message_id();
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        let user_entry = SessionLogEntry::Message {
            id: Some(user_msg_id.clone()),
            role: MessageRole::User,
            content,
            timestamp: None,
            fence_token: None,
        };
        let user_msg_seq = log
            .append_event_async(&user_entry)
            .await
            .context("failed to append user message to session log")?;

        log::info!(
            "nats session: appended user message session_id={} len={}",
            self.session_id,
            serde_json::to_string(&user_entry).map_or(0, |entry| entry.len())
        );
        Ok(AppendedPrompt {
            user_msg_id,
            user_msg_seq,
        })
    }

    async fn publish_activation(
        &self,
        user_msg_seq: u64,
        tool_confirmation_subject: Option<&str>,
    ) -> Result<()> {
        match &self.config.activation_route {
            SessionActivationRoute::ClusterShared => {
                let activation = SessionActivate::new(&self.session_id)
                    .with_tool_confirmation_subject(tool_confirmation_subject);
                publish_session_activate(&self.jetstream, &self.config.cluster, &activation)
                    .await
                    .context("failed to publish session activation")?;
            }
            SessionActivationRoute::WorkerTargeted {
                session_scope,
                worker_id,
            } => {
                let activation =
                    SessionActivate::targeted(&self.session_id, user_msg_seq, worker_id)
                        .with_tool_confirmation_subject(tool_confirmation_subject);
                publish_targeted_session_activate(
                    &self.jetstream,
                    LocalWorkerTarget::new(session_scope, worker_id)?,
                    &activation,
                )
                .await
                .context("failed to publish targeted session activation")?;
            }
        }

        log::info!(
            "nats session: published activation session_id={} cluster={}",
            self.session_id,
            self.config.cluster
        );
        Ok(())
    }

    /// Durably append a prompt and publish the matching worker activation
    /// without waiting for the target turn to finish.
    pub(crate) async fn enqueue(&self, user_message: &str) -> Result<EnqueuedPrompt> {
        let user_msg_seq = self
            .enqueue_text(user_message)
            .await?
            .into_activation_result()?;
        Ok(EnqueuedPrompt {
            session_id: self.session_id.clone(),
            user_msg_seq,
        })
    }

    /// Durably queue text for the active session without waiting for its turn
    /// to finish. A running worker can inject it at the next tool-round seam;
    /// the activation also guarantees delivery if the current turn wins the
    /// race and becomes idle first. Once the append succeeds, the returned
    /// sequence remains authoritative even if activation publication fails;
    /// callers must retry activation instead of appending the text again.
    pub async fn enqueue_text(&self, user_message: &str) -> Result<DurableTextEnqueue> {
        let appended = self
            .append_user_content(MessageContent::Text(user_message.to_string()))
            .await?;
        let activation_error = self
            .publish_activation(appended.user_msg_seq, None)
            .await
            .err();
        Ok(DurableTextEnqueue {
            user_msg_seq: appended.user_msg_seq,
            activation_error,
        })
    }

    /// Re-publish activation for an existing pending durable turn without
    /// appending another user message. This lets a newly attached frontend
    /// recover a session whose previous worker disappeared after the prompt
    /// was already stored.
    pub async fn activate_pending_turn(&self) -> Result<Option<u64>> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        let entries = log
            .load_events_latest_async()
            .await
            .context("failed to inspect pending session before activation")?;
        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
        let Some(user_msg_seq) = effective.iter().rev().find_map(|(seq, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, .. } if role.is_user()
            )
            .then_some(*seq)
        }) else {
            return Ok(None);
        };
        if requested_seq_status(&entries, user_msg_seq)? == RequestedSeqStatus::Covered {
            return Ok(None);
        }
        self.publish_activation(user_msg_seq, None).await?;
        Ok(Some(user_msg_seq))
    }

    /// Cancel the latest pending turn and wait until a lease holder confirms
    /// the durable cancellation.
    ///
    /// Re-activation is intentional: after a local worker is replaced, the new
    /// process first needs to claim the pending session and install its control
    /// subscription. The bounded retry window includes one default lease TTL,
    /// allowing a killed holder's lease to expire before its replacement
    /// writes the fenced `Cancel` entry.
    pub async fn cancel_pending_turn(&self) -> Result<bool> {
        let Some(user_msg_seq) = self.activate_pending_turn().await? else {
            return Ok(false);
        };
        let deadline = tokio::time::Instant::now() + CANCEL_RECOVERY_TIMEOUT;
        loop {
            if self.request_cancel_acknowledgement().await {
                return Ok(true);
            }
            if self.pending_turn_is_covered(user_msg_seq).await? {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for worker to durably cancel session '{}'",
                    self.session_id
                );
            }
            tokio::time::sleep(CONTROL_ACK_RETRY_DELAY).await;
        }
    }

    async fn request_cancel_acknowledgement(&self) -> bool {
        let result = request_control_command(
            &self.client,
            &self.session_id,
            &ControlCommand::Cancel,
            CONTROL_ACK_ATTEMPT_TIMEOUT,
        )
        .await;
        if let Err(error) = &result {
            log::debug!(
                "session cancel not yet acknowledged: session_id={} error={error:#}",
                self.session_id,
            );
        }
        result.is_ok()
    }

    async fn pending_turn_is_covered(&self, user_msg_seq: u64) -> Result<bool> {
        let Ok(entries) = self.load_durable_entries().await else {
            return Ok(false);
        };
        Ok(requested_seq_status(&entries, user_msg_seq)? == RequestedSeqStatus::Covered)
    }

    /// Run a turn: append user message, activate worker, stream events until completion.
    ///
    /// This is the main entry point for all frontends (CLI and TUI).
    ///
    /// # Arguments
    /// * `user_message` - The user's prompt text.
    /// * `event_sink` - Event sink to render events (AgentEventSink impl).
    /// * `pending_cancel` - Optional channel to receive cancel requests.
    ///
    /// # Returns
    /// The final assistant response text (if any), plus the sequence number of the
    /// appended user message (for retract/edit), or an error.
    pub async fn run_turn(
        &self,
        user_message: &str,
        event_sink: Arc<dyn AgentEventSink>,
        pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
    ) -> Result<NatsTurnResult> {
        self.run_turn_content(
            MessageContent::Text(user_message.to_string()),
            event_sink,
            pending_cancel,
            None,
        )
        .await
    }

    /// Run a text turn while routing `PreToolUse` approval requests from the
    /// worker to an interactive frontend.
    pub async fn run_turn_with_tool_confirmation(
        &self,
        user_message: &str,
        event_sink: Arc<dyn AgentEventSink>,
        pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
        handler: Arc<crate::nats_tool_confirmation::ToolConfirmationHandler>,
    ) -> Result<NatsTurnResult> {
        self.run_turn_content(
            MessageContent::Text(user_message.to_string()),
            event_sink,
            pending_cancel,
            Some(handler),
        )
        .await
    }

    /// Run a turn from an already-composed input. Inline and local attachment
    /// references are uploaded to JetStream before the user message is logged.
    pub async fn run_turn_input(
        &self,
        input: &crate::config::Input,
        source_dir: Option<&std::path::Path>,
        event_sink: Arc<dyn AgentEventSink>,
        pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
    ) -> Result<NatsTurnResult> {
        let mut content = input.message_content();
        crate::nats_attachments::externalize_message_attachments(
            crate::nats_attachments::AttachmentLocation::new(
                &self.jetstream,
                self.attachment_replicas,
                &self.session_id,
            ),
            &mut content,
            source_dir,
        )
        .await?;
        self.run_turn_content(content, event_sink, pending_cancel, None)
            .await
    }

    /// Run a turn from a composed input while routing `PreToolUse` approval
    /// requests from the worker to an interactive frontend.
    pub async fn run_turn_input_with_tool_confirmation(
        &self,
        input: &crate::config::Input,
        source_dir: Option<&std::path::Path>,
        event_sink: Arc<dyn AgentEventSink>,
        pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
        handler: Arc<crate::nats_tool_confirmation::ToolConfirmationHandler>,
    ) -> Result<NatsTurnResult> {
        let mut content = input.message_content();
        crate::nats_attachments::externalize_message_attachments(
            crate::nats_attachments::AttachmentLocation::new(
                &self.jetstream,
                self.attachment_replicas,
                &self.session_id,
            ),
            &mut content,
            source_dir,
        )
        .await?;
        self.run_turn_content(content, event_sink, pending_cancel, Some(handler))
            .await
    }

    async fn run_turn_content(
        &self,
        content: MessageContent,
        event_sink: Arc<dyn AgentEventSink>,
        pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
        tool_confirmation_handler: Option<
            Arc<crate::nats_tool_confirmation::ToolConfirmationHandler>,
        >,
    ) -> Result<NatsTurnResult> {
        // Step 1: Append user message to durable log BEFORE activating.
        // The worker derives input from the last user message.
        let appended = self.append_user_content(content).await?;
        let user_msg_id = appended.user_msg_id;
        let user_msg_seq = appended.user_msg_seq;

        // Step 2: Attach to session event stream (subscribe-first, then history).
        let event_stream = SessionEventStream::attach(
            self.jetstream.clone(),
            self.client.clone(),
            &self.session_id,
        )
        .await
        .context("failed to attach to session event stream")?;

        // Render history to event sink (for resume/attach scenarios)
        let history = event_stream.history();
        // Apply mutations so retracted/edited entries are excluded from history render.
        let _effective_history =
            match harnx_core::session_reconstruct::apply_log_mutations_nats(history) {
                Ok(history) => history,
                Err(err) => {
                    log::warn!(
                        "failed to apply NATS log mutations while rendering session history: {err}"
                    );
                    history.to_vec()
                }
            };
        // Front-ends render resumed history through their explicit transcript
        // loading path. Replaying it on every newly-created per-turn session
        // duplicates all prior messages in interactive and continued sessions.

        // Step 3: Subscribe the interactive frontend before publishing the
        // activation so a fast worker cannot observe an `ask` decision before
        // the confirmation bridge is ready.
        let confirmation_subject = tool_confirmation_handler
            .as_ref()
            .map(|_| self.client.new_inbox());
        let _confirmation_responder =
            match (confirmation_subject.as_ref(), tool_confirmation_handler) {
                (Some(subject), Some(handler)) => Some(
                    crate::nats_tool_confirmation::ToolConfirmationResponder::start(
                        self.client.clone(),
                        subject.clone(),
                        handler,
                    )
                    .await?,
                ),
                _ => None,
            };
        self.publish_activation(user_msg_seq, confirmation_subject.as_deref())
            .await?;

        // Step 4: Stream events and handle control until turn completion.
        let mut event_stream = event_stream;
        let mut final_response: Option<String> = None;
        let mut turn_error: Option<String> = None;
        let mut turn_complete = false;
        // Cached effective log for emitting LogSeqAssigned on live advisory
        // events. Refreshed on throttled durable reloads so we avoid O(N^2)
        // full-log load per streaming token.
        let mut cached_effective: Option<Vec<(u64, SessionLogEntry)>> = None;
        let mut pending_advisories = VecDeque::new();
        let mut emitted_logical_seqs = HashSet::new();
        // Throttle the durable-log completion check: advisory events arrive at
        // streaming-token frequency, and reloading the full log on each would be
        // O(N^2) in session size. Check at most once per interval instead — the
        // turn-complete signal (a final AssistantMessage) is not latency
        // sensitive, and the post-loop reload guarantees we never miss it.
        const COMPLETION_CHECK_INTERVAL: std::time::Duration =
            std::time::Duration::from_millis(500);
        let mut last_completion_check = std::time::Instant::now() - COMPLETION_CHECK_INTERVAL;
        let mut completion_interval = tokio::time::interval(COMPLETION_CHECK_INTERVAL);
        completion_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Wrap channel in Option for select! handling.
        let mut pending_cancel_rx = pending_cancel;
        let mut was_cancelled = false;
        let mut orphan_watchdog = SessionLeaseWatchdog::new();
        let mut saw_terminal_model_error = false;
        let mut saw_turn_ended = false;

        // Main event loop with abort signal handling
        let abort_signal_clone = self.abort_signal.clone();
        loop {
            tokio::select! {
                // Check for cancellation via wait_abort_signal
                _ = wait_abort_signal(&abort_signal_clone) => {
                    was_cancelled = true;
                    log::info!("nats session: abort signal received, publishing cancel");
                    if let Err(error) = publish_control_command(
                        &self.client,
                        &self.session_id,
                        &ControlCommand::Cancel,
                    )
                    .await
                    {
                        log::warn!("nats session: failed to publish abort control command: {error:#}");
                    }
                    break;
                }

                // Handle pending cancel requests. Publish synchronously so run_turn
                // cannot tear down before the command reaches the NATS connection.
                Some(()) = async {
                    match &mut pending_cancel_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    was_cancelled = true;
                    log::info!("nats session: pending cancel received, publishing cancel");
                    if let Err(error) = publish_control_command(
                        &self.client,
                        &self.session_id,
                        &ControlCommand::Cancel,
                    )
                    .await
                    {
                        log::warn!("nats session: failed to publish pending cancel control command: {error:#}");
                    }
                    break;
                }

                // Poll durable completion independently so a client that misses
                // the live Turn::Ended advisory still observes the authoritative
                // TurnEnd marker promptly.
                _ = completion_interval.tick() => {
                    if let Ok(entries) = self.load_durable_entries().await {
                        // Refresh cached effective log for live LogSeqAssigned.
                        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries).ok();
                        let completion_visible = Self::is_turn_completion_visible(
                            &entries,
                            effective.as_deref(),
                            user_msg_seq,
                            saw_turn_ended,
                        );
                        if let Some(effective) = effective {
                            cached_effective = Some(effective);
                            emit_all_logical_seqs_for_window(
                                cached_effective.as_deref(),
                                &event_sink,
                                &mut emitted_logical_seqs,
                            );
                            flush_pending_advisories(
                                &mut pending_advisories,
                                cached_effective.as_deref(),
                                &event_sink,
                                &mut emitted_logical_seqs,
                                AdvisoryFlush::Live,
                            );
                        }
                        if completion_visible {
                            (final_response, turn_error) =
                                Self::extract_turn_outcome(&entries, user_msg_seq);
                            turn_complete = true;
                            break;
                        }
                        if let Some(reason) = orphan_watchdog
                            .check(&self.jetstream, &self.session_id)
                            .await
                        {
                            // Workers predating durable TurnEnd release their
                            // lease after persisting the assistant row. Treat
                            // that confirmed release as their durable-compatible
                            // boundary; a missing worker with no answer remains
                            // an orphaned-turn failure.
                            (final_response, turn_error) =
                                Self::extract_turn_outcome(&entries, user_msg_seq);
                            if final_response.is_none() {
                                turn_error = Some(reason);
                            }
                            turn_complete = true;
                            break;
                        }
                    }
                }

                // Receive advisory events
                maybe_envelope = event_stream.next() => {
                    match maybe_envelope {
                        Some(envelope) => {
                            // Check if this advisory should be rendered (dedup rule)
                            if event_stream.should_render(&envelope) {
                                // Only parent-scope terminal events complete this
                                // turn. Sub-agent events remain wrapped
                                // in AgentEvent::SubAgent and therefore do not set
                                // either compatibility flag.
                                saw_terminal_model_error |= matches!(
                                    envelope.event,
                                    AgentEvent::Model(ModelEvent::Error(_))
                                );
                                let turn_ended = matches!(
                                    envelope.event,
                                    AgentEvent::Turn(TurnEvent::Ended { .. })
                                );
                                saw_turn_ended |= turn_ended;
                                pending_advisories.push_back(envelope.clone());
                                flush_pending_advisories(
                                    &mut pending_advisories,
                                    cached_effective.as_deref(),
                                    &event_sink,
                                    &mut emitted_logical_seqs,
                                    AdvisoryFlush::Live,
                                );
                            }
                            // Poll durable log for turn completion, but at most
                            // once per COMPLETION_CHECK_INTERVAL so bursty
                            // streaming advisories do not trigger full log reload
                            // each time (avoids O(N^2) growth).
                            if last_completion_check.elapsed() >= COMPLETION_CHECK_INTERVAL {
                                last_completion_check = std::time::Instant::now();
                                if let Ok(entries) = self.load_durable_entries().await {
                                    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries).ok();
                                    let completion_visible = Self::is_turn_completion_visible(
                                        &entries,
                                        effective.as_deref(),
                                        user_msg_seq,
                                        saw_turn_ended,
                                    );
                                    if let Some(effective) = effective {
                                        cached_effective = Some(effective);
                                        emit_all_logical_seqs_for_window(
                                cached_effective.as_deref(),
                                &event_sink,
                                &mut emitted_logical_seqs,
                            );
                                        flush_pending_advisories(
                                            &mut pending_advisories,
                                            cached_effective.as_deref(),
                                            &event_sink,
                                            &mut emitted_logical_seqs,
                                            AdvisoryFlush::Live,
                                        );
                                    }
                                    if completion_visible {
                                        // Extract final response before we finish
                                        (final_response, turn_error) =
                                            Self::extract_turn_outcome(&entries, user_msg_seq);
                                        turn_complete = true;
                                        break;
                                    }
                                }
                            }
                        }
                        None => {
                            // Subscription closed, check final state
                            log::info!("nats session: event stream closed");
                            break;
                        }
                    }
                }
            }
        }

        // Re-load durable log to get final state if we haven't already.
        //
        // The error is held rather than propagated with `?`: returning here would
        // skip the final flush below and lose advisories that had already been
        // received, which is the whole point of that flush. They get emitted
        // undecorated (no reload means no window), then the error is returned.
        let mut final_reload_error = None;
        if !turn_complete {
            match self.load_durable_entries().await {
                Ok(entries) => {
                    (final_response, turn_error) =
                        Self::extract_turn_outcome(&entries, user_msg_seq);
                    if let Ok(effective) =
                        harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)
                    {
                        cached_effective = Some(effective);
                        // Paired with the cache refresh everywhere else. If the
                        // loop exited before any reload succeeded — a closed
                        // subscription, or a cancel before the first completion
                        // tick — this is the first reconstruction, and without it
                        // live Message and ToolCalls rows never get their
                        // LogSeqAssigned.
                        emit_all_logical_seqs_for_window(
                            cached_effective.as_deref(),
                            &event_sink,
                            &mut emitted_logical_seqs,
                        );
                    }
                }
                Err(error) => final_reload_error = Some(error),
            }
        }

        // Render what the worker already published before reporting the turn's
        // terminal state.
        //
        // Notices — retry warnings, "exhausted retries", fallback transitions —
        // are ephemeral advisories, published from a detached task. The terminal
        // error is a durable log entry, found by polling. The two have no ordering
        // relationship, so breaking out of the loop the moment the log reads
        // complete abandoned advisories that were already in flight: they arrived
        // after the error, or were dropped with `pending_advisories` and never
        // shown at all. Losing the last "exhausted retries" line is the visible
        // case — it's the only explanation a user gets for a fallback.
        //
        // No waiting on the subscription: an earlier version of this spent up to
        // 250ms hoping in-flight advisories would land. Flushing what has already
        // been received costs nothing and never delays a completed turn.
        flush_pending_advisories(
            &mut pending_advisories,
            cached_effective.as_deref(),
            &event_sink,
            &mut emitted_logical_seqs,
            AdvisoryFlush::Final,
        );

        if let Some(error) = final_reload_error {
            return Err(error);
        }

        if !saw_terminal_model_error {
            if let Some(error) = &turn_error {
                render_error_entry(error, &event_sink);
            }
        }

        Ok(NatsTurnResult {
            response: final_response,
            session_id: self.session_id.clone(),
            was_cancelled: was_cancelled || self.abort_signal.aborted(),
            error: turn_error,
            user_msg_seq,
            user_msg_id,
        })
    }

    /// Retract (delete) a queued user message by appending an EditEntries delete.
    ///
    /// This is used to retract a user message that was appended but not yet
    /// consumed by a worker. The retract is only valid before an assistant
    /// barrier (response) appears in the log. The edit-vs-deliver race is
    /// accepted as benign per design.
    ///
    /// # Arguments
    /// * `seq` - The JetStream sequence number of the user message to retract.
    ///
    /// # Returns
    /// The sequence number of the appended EditEntries entry.
    pub async fn retract_user_message(&self, seq: u64) -> Result<u64> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        let seq = usize::try_from(seq).context("JetStream seq does not fit into usize")?;
        let edit_entry = SessionLogEntry::EditEntries {
            from: seq,
            to: seq,
            replacements: vec![], // Empty = deletion
        };
        log.append_event_async(&edit_entry)
            .await
            .context("failed to append EditEntries for retraction")
    }

    /// Edit (replace) a queued user message by appending an EditEntries replace.
    ///
    /// This is used to edit a user message that was appended but not yet
    /// consumed by a worker. The edit is only valid before an assistant
    /// barrier (response) appears in the log. The edit-vs-deliver race is
    /// accepted as benign per design.
    ///
    /// # Arguments
    /// * `seq` - The JetStream sequence number of the user message to edit.
    /// * `new_text` - The replacement user-message text.
    ///
    /// # Returns
    /// The sequence number of the appended EditEntries entry.
    pub async fn edit_user_message(&self, seq: u64, new_text: String) -> Result<u64> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        let replacement_entry = SessionLogEntry::Message {
            id: Some(new_client_message_id()),
            role: MessageRole::User,
            content: MessageContent::Text(new_text),
            timestamp: None,
            fence_token: None,
        };
        let replacement_yaml = serde_yaml::to_string(&replacement_entry)
            .context("failed to serialize replacement entry for user-message edit")?;
        let seq = usize::try_from(seq).context("JetStream seq does not fit into usize")?;
        let edit_entry = SessionLogEntry::EditEntries {
            from: seq,
            to: seq,
            replacements: vec![replacement_yaml],
        };
        log.append_event_async(&edit_entry)
            .await
            .context("failed to append EditEntries for edit")
    }

    /// Load all durable entries from the session log.
    async fn load_durable_entries(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        log.load_events_async()
            .await
            .context("failed to load durable session log")
    }

    fn has_durable_assistant_response(
        entries: &[(u64, SessionLogEntry)],
        user_msg_seq: u64,
    ) -> bool {
        Self::extract_final_response(entries, user_msg_seq).is_some()
    }

    fn is_turn_completion_visible(
        entries: &[(u64, SessionLogEntry)],
        effective: Option<&[(u64, SessionLogEntry)]>,
        user_msg_seq: u64,
        saw_turn_ended: bool,
    ) -> bool {
        let status = effective.map_or_else(
            || requested_seq_status(entries, user_msg_seq).ok(),
            |effective| {
                Some(requested_seq_status_with_effective(
                    entries,
                    effective,
                    user_msg_seq,
                ))
            },
        );
        status == Some(RequestedSeqStatus::Covered)
            || (saw_turn_ended && Self::has_durable_assistant_response(entries, user_msg_seq))
    }

    /// Final assistant text and worker-reported failure for the current turn.
    fn extract_turn_outcome(
        entries: &[(u64, SessionLogEntry)],
        user_msg_seq: u64,
    ) -> (Option<String>, Option<String>) {
        (
            Self::extract_final_response(entries, user_msg_seq),
            Self::extract_turn_error(entries, user_msg_seq),
        )
    }

    /// Worker-reported failure for the current turn, if the turn ended in one.
    fn extract_turn_error(entries: &[(u64, SessionLogEntry)], user_msg_seq: u64) -> Option<String> {
        entries
            .iter()
            .rev()
            .filter(|(seq, _)| *seq > user_msg_seq)
            .find_map(|(_, entry)| match entry {
                SessionLogEntry::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
    }

    /// Extract final assistant response text for current turn from durable entries.
    fn extract_final_response(
        entries: &[(u64, SessionLogEntry)],
        user_msg_seq: u64,
    ) -> Option<String> {
        let turn_entries: Vec<_> = entries
            .iter()
            .filter(|(seq, _)| *seq > user_msg_seq)
            .cloned()
            .collect();

        // Apply mutations so retracted messages are excluded.
        let effective_entries =
            match harnx_core::session_reconstruct::apply_log_mutations_nats(&turn_entries) {
                Ok(entries) => entries,
                Err(err) => {
                    log::warn!(
                        "failed to apply NATS log mutations while extracting final response: {err}"
                    );
                    return None;
                }
            };
        // Find the last assistant message in effective entries for this turn only.
        for (_, entry) in effective_entries.iter().rev() {
            if let SessionLogEntry::Message { role, content, .. } = entry {
                if role.is_assistant() {
                    return Some(content.to_text());
                }
            }
        }
        None
    }
}

/// Result of a NATS session turn.
#[derive(Debug, Clone)]
pub struct NatsTurnResult {
    /// Final assistant response text (if any).
    pub response: Option<String>,
    /// Session ID (for resume/attach).
    pub session_id: String,
    /// Whether the turn was cancelled.
    pub was_cancelled: bool,
    /// Worker-side failure that ended the turn without an assistant reply.
    pub error: Option<String>,
    /// Sequence number of the appended user message (for retract/edit).
    pub user_msg_seq: u64,
    /// Client-generated message ID of the appended user message.
    pub user_msg_id: String,
}

/// Render a session log entry to an event sink.
///
/// Used for rendering history when attaching to an existing session.
#[allow(dead_code)]
fn should_skip_replay_entry(seq: u64, user_msg_seq: u64) -> bool {
    seq == user_msg_seq
}

#[allow(dead_code)]
fn replay_history_to_sink(
    effective_history: &[(u64, SessionLogEntry)],
    history_window: &ActiveContextWindow<'_, (u64, SessionLogEntry)>,
    user_msg_seq: u64,
    sink: Arc<dyn AgentEventSink>,
) {
    for (seq, entry) in effective_history {
        if should_skip_replay_entry(*seq, user_msg_seq) {
            continue;
        }
        render_log_entry_to_sink(entry, *seq, history_window, sink.clone());
    }
}

#[allow(dead_code)]
fn render_log_entry_to_sink(
    entry: &SessionLogEntry,
    physical_seq: u64,
    history_window: &ActiveContextWindow<'_, (u64, SessionLogEntry)>,
    sink: Arc<dyn AgentEventSink>,
) {
    let rendered = match entry {
        SessionLogEntry::Message { role, content, .. } => {
            render_message_entry(role, content, &sink);
            true
        }
        SessionLogEntry::ToolCalls { text, calls, .. } => {
            render_tool_calls_entry(text, calls, &sink);
            true
        }
        SessionLogEntry::ToolResults { results, .. } => {
            render_tool_results_entry(results, &sink);
            false
        }
        SessionLogEntry::Cancel { .. } => {
            render_cancel_entry(&sink);
            false
        }
        SessionLogEntry::Error { message, .. } => {
            render_error_entry(message, &sink);
            false
        }
        SessionLogEntry::DataUrls { .. }
        | SessionLogEntry::Compress { .. }
        | SessionLogEntry::TurnEnd { .. }
        | SessionLogEntry::Clear
        | SessionLogEntry::EditEntries { .. }
        | SessionLogEntry::Rewind { .. }
        | SessionLogEntry::Unknown => false,
    };
    if rendered {
        for logical_index in logical_indices_for_entry(physical_seq, entry, history_window) {
            sink.emit(AgentEvent::Session(SessionEvent::LogSeqAssigned {
                seq: logical_index,
            }));
        }
    }
    // Note: ToolResults fence_token is not currently exposed in the entry type
    // (it belongs to ToolCalls which should be followed by ToolResults)
}

fn emit_live_logical_seq_for_physical(
    physical_seq: u64,
    history_window: &ActiveContextWindow<'_, (u64, SessionLogEntry)>,
    sink: &Arc<dyn AgentEventSink>,
    emitted_logical_seqs: &mut HashSet<usize>,
) -> Option<usize> {
    let logical_index = history_window
        .logical_indices_for_physical(physical_seq)
        .last()?;
    emit_deduped_logical_seq(logical_index, sink, emitted_logical_seqs);
    Some(logical_index)
}

/// Whether another flush can still happen for this turn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AdvisoryFlush {
    /// Mid-turn. With no reconstructed log to decorate against, advisories stay
    /// queued so a later flush can stamp them.
    Live,
    /// Last flush of the turn. Nothing runs after this, so holding an advisory
    /// means losing it — emit it undecorated instead.
    Final,
}

fn flush_pending_advisories(
    pending_advisories: &mut VecDeque<crate::nats_event_sink::AdvisoryEnvelope>,
    effective_entries: Option<&[(u64, SessionLogEntry)]>,
    sink: &Arc<dyn AgentEventSink>,
    emitted_logical_seqs: &mut HashSet<usize>,
    mode: AdvisoryFlush,
) {
    let window = match effective_entries {
        Some(entries) => Some(active_context_window(entries)),
        // Reconstruction failed, or no durable load has succeeded yet.
        None if mode == AdvisoryFlush::Live => return,
        None => None,
    };
    while let Some(envelope) = pending_advisories.pop_front() {
        // `after_seq` may point at a physical mutation entry that reconstruction
        // intentionally removes. Sequence decoration is therefore best-effort;
        // never hold a live streaming advisory forever merely because that
        // physical seq has no effective logical entry.
        if let Some(window) = &window {
            let _ = emit_live_logical_seq_for_physical(
                envelope.after_seq,
                window,
                sink,
                emitted_logical_seqs,
            );
        }
        if !matches!(
            envelope.event,
            AgentEvent::Session(SessionEvent::LogSeqAssigned { .. })
        ) {
            sink.emit(envelope.event);
        }
    }
}

/// Assign logical `LogSeqAssigned` seqs for every renderable row in the active
/// window, deduped. Metadata lives outside the transcript, so an uncompacted
/// transcript is authoritative from its first physical user entry.
///
/// Live worker rows (streamed assistant/tool events) are numbered via advisory
/// translation; the shared dedup set keeps this pass and that path consistent.
fn emit_all_logical_seqs_for_window(
    effective_entries: Option<&[(u64, SessionLogEntry)]>,
    sink: &Arc<dyn AgentEventSink>,
    emitted_logical_seqs: &mut HashSet<usize>,
) {
    let Some(effective_entries) = effective_entries else {
        return;
    };
    let window = active_context_window(effective_entries);
    // The logical index of a row is its position within the active window.
    // Iterate the window slice directly so the
    // logical index stays aligned even when a boundary trims a pre-window prefix.
    for (logical_index, (_js_seq, entry)) in window.entries().iter().enumerate() {
        let renders = matches!(
            entry,
            SessionLogEntry::Message { .. } | SessionLogEntry::ToolCalls { .. }
        );
        if renders {
            emit_deduped_logical_seq(logical_index, sink, emitted_logical_seqs);
        }
    }
}

fn emit_deduped_logical_seq(
    logical_index: usize,
    sink: &Arc<dyn AgentEventSink>,
    emitted_logical_seqs: &mut HashSet<usize>,
) {
    if emitted_logical_seqs.insert(logical_index) {
        sink.emit(AgentEvent::Session(SessionEvent::LogSeqAssigned {
            seq: logical_index,
        }));
    }
}

pub(crate) fn logical_indices_for_entry(
    physical_seq: u64,
    entry: &SessionLogEntry,
    history_window: &ActiveContextWindow<'_, (u64, SessionLogEntry)>,
) -> Vec<usize> {
    let logical_indices: Vec<usize> = history_window
        .logical_indices_for_physical(physical_seq)
        .collect();
    if logical_indices.len() <= 1 {
        return logical_indices;
    }
    match entry {
        SessionLogEntry::Message { role, .. } if role.is_user() => history_window
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(logical_index, (candidate_seq, candidate))| {
                (*candidate_seq == physical_seq
                    && matches!(candidate, SessionLogEntry::Message { role, .. } if role.is_user()))
                .then_some(logical_index)
            })
            .collect(),
        _ => logical_indices.into_iter().rev().take(1).collect(),
    }
}

fn render_message_entry(
    role: &harnx_core::message::MessageRole,
    content: &harnx_core::message::MessageContent,
    sink: &Arc<dyn AgentEventSink>,
) {
    if role.is_assistant() {
        use harnx_core::event::ModelEvent;
        sink.emit(AgentEvent::Model(ModelEvent::Final {
            output: content.to_text(),
            usage: Default::default(),
        }));
    } else if role.is_user() {
        sink.emit(AgentEvent::User(UserEvent::Message {
            content: content.to_text(),
        }));
    }
}

fn render_tool_calls_entry(
    text: &str,
    calls: &[harnx_core::tool::ToolCall],
    sink: &Arc<dyn AgentEventSink>,
) {
    use harnx_core::event::{ContentBlock, ModelEvent, ToolEvent, ToolKind};

    for call in calls {
        sink.emit(AgentEvent::Tool(ToolEvent::Started {
            id: call.id.clone().unwrap_or_default(),
            name: call.name.clone(),
            kind: ToolKind::Other,
            markdown: None,
            input: call.arguments.clone(),
            locations: vec![],
        }));
    }

    if !text.is_empty() {
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text(text.to_string())],
        }));
    }
}

fn render_tool_results_entry(
    results: &[harnx_core::session::ToolOutput],
    sink: &Arc<dyn AgentEventSink>,
) {
    use harnx_core::event::ToolEvent;

    for result in results {
        sink.emit(AgentEvent::Tool(ToolEvent::Completed {
            id: result.id.clone().unwrap_or_default(),
            output: result.output.clone(),
            markdown: result.markdown.clone(),
        }));
    }
}

fn render_cancel_entry(sink: &Arc<dyn AgentEventSink>) {
    use harnx_core::event::NoticeEvent;

    sink.emit(AgentEvent::Notice(NoticeEvent::Warning(
        "Session cancelled".to_string(),
    )));
}

fn render_error_entry(message: &str, sink: &Arc<dyn AgentEventSink>) {
    use harnx_core::event::NoticeEvent;

    sink.emit(AgentEvent::Notice(NoticeEvent::Error(message.to_string())));
}

/// Send a control command to a remote session.
///
/// Helper for frontends to send cancel/set-pending without full NatsSession.
pub async fn send_control_command(
    client: &async_nats::Client,
    session_id: &str,
    command: ControlCommand,
) -> Result<()> {
    publish_control_command(client, session_id, &command).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct TestSink {
        count: Arc<AtomicUsize>,
        events: Mutex<Vec<AgentEvent>>,
    }
    impl AgentEventSink for TestSink {
        fn emit(&self, event: AgentEvent) {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.events.lock().unwrap().push(event);
        }
    }

    /// A turn's last flush must not swallow notices just because the durable log
    /// could not be reconstructed. Holding them there loses them, and losing the
    /// "exhausted retries" warning is the whole reason this queue gets flushed.
    #[test]
    fn final_flush_emits_advisories_when_the_log_cannot_be_reconstructed() {
        use harnx_core::event::NoticeEvent;

        let count = Arc::new(AtomicUsize::new(0));
        let sink: Arc<dyn AgentEventSink> = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });
        let mut pending = VecDeque::from(vec![crate::nats_event_sink::AdvisoryEnvelope::new(
            7,
            AgentEvent::Notice(NoticeEvent::Warning("exhausted retries".to_string())),
        )]);
        let mut emitted = HashSet::new();

        // Live: nothing to decorate against, so wait for a later flush.
        flush_pending_advisories(&mut pending, None, &sink, &mut emitted, AdvisoryFlush::Live);
        assert_eq!(pending.len(), 1, "live flush must retain, not drop");
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "live flush must not emit yet"
        );

        // Final: no later flush exists, so emit undecorated rather than lose it.
        flush_pending_advisories(
            &mut pending,
            None,
            &sink,
            &mut emitted,
            AdvisoryFlush::Final,
        );
        assert!(pending.is_empty(), "final flush must drain the queue");
        // The point of the test: drained AND delivered, not drained by dropping.
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "final flush must emit the notice, not discard it"
        );
    }

    fn test_entries(entries: &[(u64, MessageRole, &str)]) -> Vec<(u64, SessionLogEntry)> {
        entries
            .iter()
            .map(|(seq, role, text)| {
                (
                    *seq,
                    SessionLogEntry::Message {
                        id: None,
                        role: *role,
                        content: MessageContent::Text((*text).to_string()),
                        timestamp: None,
                        fence_token: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn assistant_message_is_not_a_durable_turn_end() {
        let entries = test_entries(&[
            (1, MessageRole::User, "question"),
            (2, MessageRole::Assistant, "intermediate stop-hook response"),
        ]);

        assert!(
            !NatsSession::is_turn_completion_visible(&entries, None, 1, false),
            "an assistant row may still be followed by stop-hook or pending-context work"
        );
        assert!(
            NatsSession::is_turn_completion_visible(&entries, None, 1, true),
            "older workers use the parent turn advisory as a compatibility fallback"
        );

        let mut durably_ended = entries;
        durably_ended.push((
            3,
            SessionLogEntry::TurnEnd {
                through_seq: 1,
                fence_token: 7,
                timestamp: None,
            },
        ));
        assert!(NatsSession::is_turn_completion_visible(
            &durably_ended,
            None,
            1,
            false
        ));
    }

    #[test]
    fn turn_end_does_not_complete_a_later_queued_user() {
        let entries = vec![
            (
                1,
                SessionLogEntry::Message {
                    id: None,
                    role: MessageRole::User,
                    content: MessageContent::Text("first".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                2,
                SessionLogEntry::Message {
                    id: None,
                    role: MessageRole::User,
                    content: MessageContent::Text("queued".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                3,
                SessionLogEntry::TurnEnd {
                    through_seq: 1,
                    fence_token: 7,
                    timestamp: None,
                },
            ),
        ];
        assert!(!NatsSession::is_turn_completion_visible(
            &entries, None, 2, false
        ));
    }

    #[test]
    fn durable_error_and_cancel_are_terminal_without_an_advisory() {
        let mut failed = test_entries(&[(1, MessageRole::User, "question")]);
        failed.push((
            2,
            SessionLogEntry::Error {
                message: "worker failed".to_string(),
                fence_token: 7,
                timestamp: None,
            },
        ));
        assert_eq!(
            requested_seq_status(&failed, 1).unwrap(),
            RequestedSeqStatus::Covered
        );

        let mut cancelled = test_entries(&[(1, MessageRole::User, "question")]);
        cancelled.push((2, SessionLogEntry::Cancel { fence_token: 7 }));
        assert_eq!(
            requested_seq_status(&cancelled, 1).unwrap(),
            RequestedSeqStatus::Covered
        );
    }

    #[test]
    fn retracted_request_is_covered_only_when_no_replacement_or_pending_work_remains() {
        let mut retracted = test_entries(&[(1, MessageRole::User, "withdrawn")]);
        retracted.push((
            2,
            SessionLogEntry::EditEntries {
                from: 1,
                to: 1,
                replacements: vec![],
            },
        ));
        assert_eq!(
            requested_seq_status(&retracted, 1).unwrap(),
            RequestedSeqStatus::Covered
        );

        let replacement = serde_yaml::to_string(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("replacement".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .unwrap();
        let mut replaced = test_entries(&[(1, MessageRole::User, "original")]);
        replaced.push((
            2,
            SessionLogEntry::EditEntries {
                from: 1,
                to: 1,
                replacements: vec![replacement],
            },
        ));
        assert_eq!(
            requested_seq_status(&replaced, 1).unwrap(),
            RequestedSeqStatus::Pending
        );
    }

    #[test]
    fn test_render_message_entry() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });

        // Test assistant message
        let entry = SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("Hello".to_string()),
            timestamp: None,
            fence_token: None,
        };
        let empty: Vec<(u64, SessionLogEntry)> = vec![];
        let window = active_context_window(&empty);
        render_log_entry_to_sink(&entry, 0, &window, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Test user message
        let entry = SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("Hi".to_string()),
            timestamp: None,
            fence_token: None,
        };
        let empty: Vec<(u64, SessionLogEntry)> = vec![];
        let window = active_context_window(&empty);
        render_log_entry_to_sink(&entry, 0, &window, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_replay_history_skips_last_user_message_only() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });

        let effective_history = test_entries(&[
            (1, MessageRole::User, "first user"),
            (2, MessageRole::Assistant, "assistant"),
            (3, MessageRole::User, "current user"),
        ]);

        let window = active_context_window(&effective_history);
        replay_history_to_sink(&effective_history, &window, 3, sink.clone());

        // Metadata is outside the transcript, so the first user row is logical
        // zero and sequence assignment is immediately authoritative.
        assert_eq!(count.load(Ordering::SeqCst), 4);
        let rendered_messages: Vec<String> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::User(UserEvent::Message { content }) => Some(content.clone()),
                AgentEvent::Model(harnx_core::event::ModelEvent::Final { output, .. }) => {
                    Some(output.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(rendered_messages, vec!["first user", "assistant"]);
        assert!(!rendered_messages.iter().any(|msg| msg == "current user"));
        let replayed_seqs: Vec<usize> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) => Some(*seq),
                _ => None,
            })
            .collect();
        assert_eq!(replayed_seqs, vec![0, 1]);
    }

    #[test]
    fn test_replay_without_compaction_emits_row_seqs() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });

        let effective_history = test_entries(&[
            (1, MessageRole::User, "first user"),
            (2, MessageRole::Assistant, "assistant"),
            (3, MessageRole::User, "current user"),
        ]);

        let window = active_context_window(&effective_history);
        replay_history_to_sink(&effective_history, &window, 3, sink.clone());

        let replayed_seqs: Vec<usize> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) => Some(*seq),
                _ => None,
            })
            .collect();
        assert_eq!(replayed_seqs, vec![0, 1]);
    }

    #[test]
    fn test_emit_all_logical_seqs_numbers_every_renderable_window_row() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });
        let sink_trait: Arc<dyn AgentEventSink> = sink.clone();
        let user_msg_id = "live-user".to_string();
        let effective_history = vec![
            (
                3,
                SessionLogEntry::Message {
                    id: Some("legacy-user".to_string()),
                    role: MessageRole::User,
                    content: MessageContent::Text("legacy".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                3,
                SessionLogEntry::Message {
                    id: Some(user_msg_id.clone()),
                    role: MessageRole::User,
                    content: MessageContent::Text("live user".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                4,
                SessionLogEntry::Message {
                    id: Some("assistant".to_string()),
                    role: MessageRole::Assistant,
                    content: MessageContent::Text("reply".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
        ];
        let mut emitted_logical_seqs = HashSet::new();

        emit_all_logical_seqs_for_window(
            Some(&effective_history),
            &sink_trait,
            &mut emitted_logical_seqs,
        );

        // Every message row is numbered by its logical position, deduped and
        // in order.
        let assigned_seqs: Vec<usize> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) => Some(*seq),
                _ => None,
            })
            .collect();
        assert_eq!(assigned_seqs, vec![0, 1, 2]);
    }

    #[test]
    fn test_logical_indices_for_entry_user_fans_out_shared_seq() {
        let effective_history = vec![
            (
                51,
                SessionLogEntry::Message {
                    id: Some("u1".to_string()),
                    role: MessageRole::User,
                    content: MessageContent::Text("u1".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                51,
                SessionLogEntry::Message {
                    id: Some("u2".to_string()),
                    role: MessageRole::User,
                    content: MessageContent::Text("u2".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                51,
                SessionLogEntry::Message {
                    id: Some("a1".to_string()),
                    role: MessageRole::Assistant,
                    content: MessageContent::Text("a1".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
        ];
        let window = active_context_window(&effective_history);

        let user_indices = logical_indices_for_entry(51, &effective_history[0].1, &window);
        let assistant_indices = logical_indices_for_entry(51, &effective_history[2].1, &window);

        assert_eq!(user_indices, vec![0, 1]);
        assert_eq!(assistant_indices, vec![2]);
    }
    #[test]
    fn test_render_tool_results_entry() {
        use harnx_core::event::ToolEvent;
        use harnx_core::session::ToolOutput;

        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });

        let entry = SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("test".to_string()),
                name: "echo".to_string(),
                output: serde_json::json!({"result": "ok"}),
                markdown: Some("rendered summary".to_string()),
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        };
        let empty: Vec<(u64, SessionLogEntry)> = vec![];
        let window = active_context_window(&empty);
        render_log_entry_to_sink(&entry, 0, &window, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Tool(ToolEvent::Completed { markdown, .. }) => {
                assert_eq!(markdown.as_deref(), Some("rendered summary"));
            }
            other => panic!("expected Completed event, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_final_response_ignores_stale_prior_turn_reply() {
        let entries = test_entries(&[
            (1, MessageRole::User, "old prompt"),
            (2, MessageRole::Assistant, "old"),
            (3, MessageRole::User, "new prompt"),
            (4, MessageRole::Assistant, "new"),
        ]);

        assert_eq!(
            NatsSession::extract_final_response(&entries, 3),
            Some("new".to_string())
        );
    }

    #[test]
    fn test_extract_final_response_returns_none_without_new_assistant_reply() {
        let entries = test_entries(&[
            (1, MessageRole::User, "old prompt"),
            (2, MessageRole::Assistant, "old"),
            (3, MessageRole::User, "new prompt"),
        ]);

        assert_eq!(NatsSession::extract_final_response(&entries, 3), None);
    }
}
