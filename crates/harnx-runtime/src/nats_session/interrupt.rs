//! Interruption is one fenced append. Nothing here waits for propagation.
use super::NatsSession;
use crate::nats_session_log::{FencedAppend, NatsSessionLog};
use crate::nats_worker::{
    publish_control_command, publish_session_activate, publish_targeted_session_activate,
    ControlCommand, LocalWorkerTarget, SessionActivate, SessionActivationRoute,
};
use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::{current_turn_entries, last_terminator_is_cancel};
use std::time::Duration;

/// How often [`await_prompt_interrupt`] re-reads the log while a turn runs.
const PROMPT_INTERRUPT_POLL: Duration = Duration::from_millis(250);

/// How many consecutive failed reads the interrupt watch absorbs before it
/// gives up. A broker hiccup must not be reported as an interruption, and a
/// broker that is really gone must still be reported.
const PROMPT_INTERRUPT_READ_ATTEMPTS: usize = 5;

/// The append has two seconds to land. Longer than that is a broker problem a
/// caller should hear about rather than keep waiting on.
const INTERRUPT_APPEND_TIMEOUT: Duration = Duration::from_secs(2);

pub struct InterruptRequest {
    /// Storage key of the session (`session_key(agent, id)`).
    pub session_id: String,
    /// Cluster key used for a cluster-shared activation route.
    pub cluster: String,
    pub cancellation_id: String,
    pub requested_by: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum InterruptOutcome {
    Idle,
    Accepted { cancel_seq: u64 },
    AlreadyInterrupted { cancel_seq: u64 },
}

impl InterruptOutcome {
    /// The sequence the `Cancel` landed at (or already occupied), when this
    /// outcome names one. A caller fences live output by it
    /// (`LiveEventState::accept_interrupt`); `Idle` names no `Cancel` at all.
    pub fn cancel_seq(&self) -> Option<u64> {
        match self {
            InterruptOutcome::Idle => None,
            InterruptOutcome::Accepted { cancel_seq }
            | InterruptOutcome::AlreadyInterrupted { cancel_seq } => Some(*cancel_seq),
        }
    }
}

impl NatsSession {
    /// Bind the session that invoked this one as a sub-agent child. The
    /// invocation id is what replay keys off, and the parent session id names
    /// who asked for an interruption this child appends.
    pub fn with_execution_parent(
        mut self,
        parent_session_id: String,
        invocation_id: String,
    ) -> Self {
        self.parent_session_id = Some(parent_session_id);
        self.invocation_id = Some(invocation_id);
        self
    }

    /// Interrupting is one fenced `Cancel` append to this session's log. The
    /// log is the sole authority: acceptance means the append landed, not
    /// that any worker has observed it yet.
    pub async fn interrupt(&self, reason: &str) -> Result<InterruptOutcome> {
        self.abort_signal.set_ctrlc();
        let request = InterruptRequest {
            session_id: self.storage_key.clone(),
            cluster: self.config.cluster.clone(),
            cancellation_id: uuid::Uuid::now_v7().to_string(),
            requested_by: self.requester_label(),
            reason: reason.into(),
        };
        tokio::time::timeout(
            INTERRUPT_APPEND_TIMEOUT,
            interrupt_session(
                &self.jetstream,
                &self.client,
                &self.config.activation_route,
                request,
            ),
        )
        .await
        .context("timed out appending the interrupt; retry")?
    }

    /// Return on durable acceptance, reporting whether there was a turn to
    /// interrupt. Winding the interrupted turn up is the worker's job and
    /// never delays a caller's return.
    pub async fn cancel_pending_turn(&self) -> Result<bool> {
        Ok(!matches!(
            self.interrupt("client cancel").await?,
            InterruptOutcome::Idle
        ))
    }

    /// Wait until the log carries a `Cancel` that ended the turn the prompt at
    /// `user_msg_seq` started, returning the sequence it landed at. A prompt
    /// follower uses this to return the moment its turn is interrupted, rather
    /// than when a worker gets around to noticing, and to fence its live
    /// output (`LiveEventState::accept_interrupt`) by the same sequence. A
    /// `Cancel` at or below `user_msg_seq` belongs to an earlier turn — this
    /// prompt was typed after the interruption, not stopped by it.
    pub(super) async fn wait_for_prompt_interrupt(&self, user_msg_seq: u64) -> Result<u64> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.storage_key.clone());
        await_prompt_interrupt(user_msg_seq, move || {
            let log = log.clone();
            async move { log.load_events_after_async(user_msg_seq).await }
        })
        .await
    }

    /// The sequence of the `Cancel` that interrupted the prompt at
    /// `user_msg_seq`, if the log carries one yet.
    pub(super) async fn prompt_interrupt_seq(&self, user_msg_seq: u64) -> Result<Option<u64>> {
        let entries = NatsSessionLog::new(self.jetstream.clone(), self.storage_key.clone())
            .load_events_after_async(user_msg_seq)
            .await
            .context("failed to load the session log above the prompt")?;
        Ok(harnx_core::session_reconstruct::prompt_interrupted_at(
            &entries,
            user_msg_seq,
        ))
    }

    /// The session that invoked this one as a sub-agent child, if any.
    fn execution_parent_session(&self) -> Option<String> {
        self.parent_session_id.clone()
    }

    fn client_instance_label(&self) -> String {
        std::process::id().to_string()
    }

    fn requester_label(&self) -> String {
        match &self.invocation_id {
            Some(_) => format!(
                "parent:{}",
                self.execution_parent_session().unwrap_or_default()
            ),
            None => format!("client:{}", self.client_instance_label()),
        }
    }

    /// Local worker ids change across restarts, so a wind-up or resume
    /// activation addressed to a dead worker is discarded. Frontends call this
    /// when they attach to a session (spec §5, "Attach behaviour").
    pub async fn republish_pending_activation(&self) -> Result<bool> {
        let entries = self.load_durable_entries().await?;
        let state = harnx_core::session_reconstruct::reconstruct_state_from_nats(&entries);
        let requested_seq = match state.turn_status {
            harnx_core::session_reconstruct::TurnStatus::Idle => return Ok(false),
            harnx_core::session_reconstruct::TurnStatus::InterruptedPendingWindUp {
                cancel_seq,
                ..
            } => cancel_seq,
            harnx_core::session_reconstruct::TurnStatus::InFlightResumable { .. } => entries
                .iter()
                .rev()
                .find_map(|(seq, e)| {
                    matches!(e, SessionLogEntry::Message { role, .. } if role.is_user())
                        .then_some(*seq)
                })
                .unwrap_or(0),
        };
        self.publish_control_activation(requested_seq, None, None)
            .await?;
        Ok(true)
    }
}

/// Wait for the `Cancel` that ended the turn the prompt at `user_msg_seq`
/// started, re-reading the log through `read` every [`PROMPT_INTERRUPT_POLL`].
///
/// `read` hands back the entries ABOVE the prompt, which is all
/// `prompt_interrupted_at` ever inspects. Reading the whole transcript
/// instead costs one broker round trip per entry the turn cannot be
/// interrupted by, every quarter second, for every follower of the turn.
///
/// A failed read is a broker hiccup, not an answer. Both callers race this
/// against the turn itself, so propagating the first failure ends a perfectly
/// healthy turn as though it had been interrupted. A bounded number of
/// consecutive failures is retried before the error is passed on; a broker
/// that is really gone still surfaces, one poll interval later.
pub async fn await_prompt_interrupt<F, Fut>(user_msg_seq: u64, mut read: F) -> Result<u64>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<(u64, SessionLogEntry)>>>,
{
    let mut consecutive_failures = 0usize;
    loop {
        match read().await {
            Ok(entries) => {
                consecutive_failures = 0;
                if let Some(cancel_seq) =
                    harnx_core::session_reconstruct::prompt_interrupted_at(&entries, user_msg_seq)
                {
                    return Ok(cancel_seq);
                }
            }
            Err(error) => {
                consecutive_failures += 1;
                if consecutive_failures >= PROMPT_INTERRUPT_READ_ATTEMPTS {
                    return Err(error);
                }
                log::debug!(
                    "interrupt watch retrying a failed log read \
                     ({consecutive_failures}/{PROMPT_INTERRUPT_READ_ATTEMPTS}): {error:#}"
                );
            }
        }
        tokio::time::sleep(PROMPT_INTERRUPT_POLL).await;
    }
}

const MAX_CAS_ATTEMPTS: usize = 16;

/// Read the session log tail, classify the current turn, and append exactly
/// one `Cancel` entry with a fenced (expected-last-sequence) publish. A tail
/// conflict is re-examined, never blindly retried: newer entries are folded
/// into our view and the turn is reclassified before trying again.
pub async fn interrupt_session(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    route: &SessionActivationRoute,
    request: InterruptRequest,
) -> Result<InterruptOutcome> {
    let log = NatsSessionLog::new(js.clone(), request.session_id.clone());
    let mut entries = log.load_events_latest_async().await?;
    for _ in 0..MAX_CAS_ATTEMPTS {
        if let Some(outcome) = classify(&entries) {
            return Ok(outcome);
        }
        let tail = entries.last().map_or(0, |(seq, _)| *seq);
        let entry = SessionLogEntry::cancel_request(
            request.cancellation_id.clone(),
            request.requested_by.clone(),
        );
        match log
            .append_fenced(&entry, tail, &request.cancellation_id)
            .await?
        {
            FencedAppend::Appended(cancel_seq) => {
                log::info!(
                    "nats session: interrupt appended session_id={} requested_by={} reason={} cancel_seq={}",
                    request.session_id,
                    request.requested_by,
                    request.reason,
                    cancel_seq
                );
                announce(js, client, route, Accepted::new(&request, cancel_seq));
                return Ok(InterruptOutcome::Accepted { cancel_seq });
            }
            FencedAppend::Conflict { entries: newer } => {
                // Our own lost ack shows up as a Cancel carrying our id.
                if let Some((seq, _)) = newer.iter().find(|(_, e)| {
                    matches!(
                        e,
                        SessionLogEntry::Cancel { cancellation_id: Some(id), .. }
                            if *id == request.cancellation_id
                    )
                }) {
                    let cancel_seq = *seq;
                    announce(js, client, route, Accepted::new(&request, cancel_seq));
                    return Ok(InterruptOutcome::Accepted { cancel_seq });
                }
                entries.extend(newer);
            }
        }
    }
    anyhow::bail!(
        "session log tail kept moving; interrupt not appended after {MAX_CAS_ATTEMPTS} attempts"
    )
}

/// `None` means "append a Cancel now".
fn classify(entries: &[(u64, SessionLogEntry)]) -> Option<InterruptOutcome> {
    let turn = current_turn_entries(entries);
    let has_user = turn
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Message { role, .. } if role.is_user()));
    if has_user {
        return None;
    }
    if last_terminator_is_cancel(entries) {
        let cancel_seq = entries
            .iter()
            .rev()
            .find_map(|(seq, e)| matches!(e, SessionLogEntry::Cancel { .. }).then_some(*seq))?;
        return Some(InterruptOutcome::AlreadyInterrupted { cancel_seq });
    }
    Some(InterruptOutcome::Idle)
}

/// The pieces of an accepted interrupt that `announce` needs: who and what
/// the request named, and the sequence the `Cancel` entry landed at. Owned,
/// because the announcement outlives the call that accepted the interrupt.
struct Accepted {
    session_id: String,
    cluster: String,
    cancellation_id: String,
    cancel_seq: u64,
}

impl Accepted {
    fn new(request: &InterruptRequest, cancel_seq: u64) -> Self {
        Self {
            session_id: request.session_id.clone(),
            cluster: request.cluster.clone(),
            cancellation_id: request.cancellation_id.clone(),
            cancel_seq,
        }
    }
}

/// Start the best-effort wake-ups an accepted interrupt owes: the control
/// hint for a live worker and a wind-up activation so a dead worker's session
/// still winds up.
///
/// They run on a task of their own because acceptance is the append and
/// nothing else. Awaiting a JetStream activation ack here puts it inside
/// whatever budget the caller gave the interrupt — `NatsSession::interrupt`
/// allows two seconds — and a slow broker would then report a failed
/// interrupt for a `Cancel` that is already durable.
fn announce(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    route: &SessionActivationRoute,
    accepted: Accepted,
) {
    let js = js.clone();
    let client = client.clone();
    let route = route.clone();
    tokio::spawn(async move {
        let hint = ControlCommand::Interrupt {
            cancellation_id: accepted.cancellation_id.clone(),
        };
        if let Err(error) = publish_control_command(&client, &accepted.session_id, &hint).await {
            log::debug!("interrupt hint not published: {error:#}");
        }
        let result = match &route {
            SessionActivationRoute::ClusterShared => {
                let activation = SessionActivate::new(&accepted.session_id)
                    .with_requested_seq(accepted.cancel_seq);
                publish_session_activate(&js, &accepted.cluster, &activation).await
            }
            SessionActivationRoute::WorkerTargeted {
                session_scope,
                worker_id,
            } => {
                let activation =
                    SessionActivate::targeted(&accepted.session_id, accepted.cancel_seq, worker_id);
                match LocalWorkerTarget::new(session_scope, worker_id) {
                    Ok(target) => publish_targeted_session_activate(&js, target, &activation).await,
                    Err(error) => Err(error),
                }
            }
        };
        if let Err(error) = result {
            log::debug!("wind-up activation not published: {error:#}");
        }
    });
}
