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
use std::future::Future;
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
    pub replicas: usize,
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

    pub async fn interrupt_prompt(
        &self,
        prompt_seq: u64,
        reason: &str,
    ) -> Result<InterruptOutcome> {
        // Only the invocation currently holding the admission head can own
        // rows appended past this prompt's own terminator (strict steering,
        // issue #2774): a stale stop for a superseded prompt must not reach
        // a later live invocation's rows, and reports idle exactly as
        // before. The gate below decides the initial ownership; the stop
        // carries the owning invocation id so the append loop re-derives
        // ownership from the admission head on every CAS attempt. The log
        // alone cannot close that window: once a successor invocation's
        // prompt row is appended, `steered_rows_outstanding` cannot tell
        // its rows from this prompt's steered ones, so head ownership is
        // what keeps a stale owner's Cancel off the successor's work.
        let drain_owner = self.prompt_drain_owner(prompt_seq).await?;
        let request = InterruptRequest {
            session_id: self.storage_key.clone(),
            cluster: self.config.cluster.clone(),
            replicas: self.attachment_replicas,
            cancellation_id: format!("prompt-stop:{prompt_seq}"),
            requested_by: self.requester_label(),
            reason: reason.into(),
        };
        tokio::time::timeout(
            INTERRUPT_APPEND_TIMEOUT,
            interrupt_invocation_stop_steered(
                &self.jetstream,
                &self.client,
                &self.config.activation_route,
                request,
                InvocationStop {
                    prompt_seq,
                    owning_head: drain_owner,
                },
            ),
        )
        .await
        .context("invocation cancellation unconfirmed; external cleanup unknown")?
    }

    /// The invocation whose steered drain `prompt_seq`'s stop may reach: the
    /// prompt's binding's invocation, when that invocation still holds the
    /// admission head. A prompt whose invocation no longer holds the head
    /// has no live drain of its own to stop. The check resolves the
    /// prompt's own binding and compares it to the head — the head
    /// invocation's newest bound row would be a steered row, not this
    /// prompt. The gate loads the durable log once and the append loop
    /// loads it again on purpose: the loop reads through its own log
    /// handle whose replica configuration serves the plain interrupt
    /// path too, so reusing the gate's entries would couple the two read
    /// configurations for a load that is already on the stop's bounded
    /// path (issue #2774 review F3).
    pub(super) async fn prompt_drain_owner(&self, prompt_seq: u64) -> Result<Option<String>> {
        bound_ownership_read(async {
            let Some(prompt_id) =
                prompt_message_id(&self.load_durable_entries().await?, prompt_seq)
            else {
                // Id-less row: nothing was ever admitted for it, so no
                // invocation can own a drain past it.
                return Ok(None);
            };
            let Some(invocation) = self
                .metadata_store
                .prompt_admission_binding(&self.storage_key, &prompt_id)
                .await?
            else {
                // Unbound row: it never joined an invocation's drain.
                return Ok(None);
            };
            let owns = self
                .metadata_store()
                .admission_head(&self.storage_key)
                .await?
                .as_deref()
                == Some(invocation.as_str());
            Ok(owns.then_some(invocation))
        })
        .await
    }

    pub async fn interrupt_admitted_invocation(&self, reason: &str) -> Result<InterruptOutcome> {
        let id = self
            .invocation_id
            .as_ref()
            .context("automatic cancellation has no invocation identity")?;
        let entries = self.load_durable_entries().await?;
        let Some(prompt_seq) = self
            .metadata_store
            .invocation_prompt_seq(&self.storage_key, id, &entries)
            .await?
        else {
            return Ok(InterruptOutcome::Idle);
        };
        // Only the invocation currently holding the admission head can own
        // rows appended past its own terminator (strict steering, issue
        // #2774): those are its steered drain, and stopping the invocation
        // must append a Cancel past the covering TurnEnd. A superseded
        // historical invocation is reported idle exactly as before, so a
        // later live invocation in the same session cannot be cancelled by
        // a stale stop. The stop carries the invocation id so the append
        // loop re-derives head ownership on every CAS attempt. The head
        // read is bounded like every other ownership-gate read: a stalled
        // broker fails the stop as an unconfirmed error instead of
        // hanging it (issue #2774 review F1).
        let head =
            bound_ownership_read(self.metadata_store().admission_head(&self.storage_key)).await?;
        let owns_head = head.as_deref() == Some(id.as_str());
        let request = InterruptRequest {
            session_id: self.storage_key.clone(),
            cluster: self.config.cluster.clone(),
            replicas: self.attachment_replicas,
            cancellation_id: format!("invocation-stop:{id}"),
            requested_by: self.requester_label(),
            reason: reason.into(),
        };
        tokio::time::timeout(
            INTERRUPT_APPEND_TIMEOUT,
            interrupt_invocation_stop_steered(
                &self.jetstream,
                &self.client,
                &self.config.activation_route,
                request,
                InvocationStop {
                    prompt_seq,
                    owning_head: owns_head.then(|| id.to_string()),
                },
            ),
        )
        .await
        .context("invocation cancellation unconfirmed; external cleanup unknown")?
    }

    /// Interrupting is one fenced `Cancel` append to this session's log. The
    /// log is the sole authority: acceptance means the append landed, not
    /// that any worker has observed it yet.
    pub async fn interrupt(&self, reason: &str) -> Result<InterruptOutcome> {
        self.abort_signal.set_ctrlc();
        let request = InterruptRequest {
            session_id: self.storage_key.clone(),
            cluster: self.config.cluster.clone(),
            replicas: self.attachment_replicas,
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
    pub(super) async fn wait_for_prompt_interrupt(
        &self,
        user_msg_seq: u64,
    ) -> Result<(u64, Option<String>)> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.storage_key.clone());
        await_prompt_interrupt_receipt(user_msg_seq, move || {
            let log = log.clone();
            async move { log.load_events_after_async(user_msg_seq).await }
        })
        .await
    }

    /// The sequence of the `Cancel` that interrupted the prompt at
    /// `user_msg_seq`, if the log carries one yet.
    pub(super) async fn prompt_interrupt_receipt(
        &self,
        user_msg_seq: u64,
    ) -> Result<Option<(u64, Option<String>)>> {
        let entries = NatsSessionLog::new(self.jetstream.clone(), self.storage_key.clone())
            .load_events_after_async(user_msg_seq)
            .await
            .context("failed to load the session log above the prompt")?;
        Ok(interrupt_receipt(&entries, user_msg_seq))
    }

    #[cfg(test)]
    pub(super) async fn prompt_interrupt_seq(&self, user_msg_seq: u64) -> Result<Option<u64>> {
        Ok(self
            .prompt_interrupt_receipt(user_msg_seq)
            .await?
            .map(|receipt| receipt.0))
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
            harnx_core::session_reconstruct::TurnStatus::Idle => {
                let Some(intent) = self
                    .metadata_store
                    .active_admission(&self.storage_key, &entries)
                    .await?
                else {
                    return Ok(false);
                };
                let id = intent.invocation_id.as_str();
                self.metadata_store
                    .bind_prompt_admission(&self.storage_key, id, id)
                    .await?;
                let prompt = match intent.prompt_content.clone() {
                    Some(content) => {
                        self.append_user_content_unchecked(content, id.to_owned())
                            .await?
                            .user_msg_seq
                    }
                    None => {
                        let log = NatsSessionLog::new(self.jetstream.clone(), &self.storage_key);
                        log.append_event_with_message_id_async(
                            &SessionLogEntry::compact_request(
                                id.to_owned(),
                                Some("admission recovery".into()),
                            ),
                            id,
                        )
                        .await?
                    }
                };
                self.publish_control_activation(prompt, None, intent.token_budget)
                    .await?;
                return Ok(true);
            }
            harnx_core::session_reconstruct::TurnStatus::InterruptedPendingWindUp {
                cancel_seq,
                ..
            } => cancel_seq,
            harnx_core::session_reconstruct::TurnStatus::InFlightResumable { .. } => {
                harnx_core::session_reconstruct::pending_prompt_seq(&entries)
                    .or_else(|| harnx_core::session_reconstruct::latest_prompt_seq(&entries))
                    .unwrap_or(0)
            }
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
pub async fn await_prompt_interrupt<F, Fut>(user_msg_seq: u64, read: F) -> Result<u64>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<(u64, SessionLogEntry)>>>,
{
    Ok(await_prompt_interrupt_receipt(user_msg_seq, read).await?.0)
}

fn interrupt_receipt(
    entries: &[(u64, SessionLogEntry)],
    prompt: u64,
) -> Option<(u64, Option<String>)> {
    let seq = harnx_core::session_reconstruct::prompt_interrupted_at(entries, prompt)?;
    let timeout = entries.iter().find_map(|(entry_seq, entry)| match entry {
        SessionLogEntry::Cancel {
            requested_by: Some(label),
            ..
        } if *entry_seq == seq && crate::parse_timeout_terminal(label).is_some() => {
            Some(label.clone())
        }
        _ => None,
    });
    Some((seq, timeout))
}

/// Wait for a prompt's interrupt fence and preserve its deadline error, if any.
/// Manual cancellation returns no error message.
pub async fn await_prompt_interrupt_receipt<F, Fut>(
    user_msg_seq: u64,
    mut read: F,
) -> Result<(u64, Option<String>)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<(u64, SessionLogEntry)>>>,
{
    let mut consecutive_failures = 0usize;
    loop {
        match read().await {
            Ok(entries) => {
                consecutive_failures = 0;
                if let Some(receipt) = interrupt_receipt(&entries, user_msg_seq) {
                    return Ok(receipt);
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

/// Read the log's last entry, decide, and append exactly one `Cancel` entry
/// with a fenced (expected-last-sequence) publish. A tail conflict is
/// re-examined, never blindly retried: newer entries are folded into our
/// view and the decision is made again before trying again.
///
/// The last entry is all the request reads: it is the tail the append is
/// fenced on and the entry `decide` classifies, so the transcript's length
/// never sits on the interrupt's critical path. A conflict brings back
/// everything appended past it, so the window only ever grows forward.
pub async fn interrupt_session(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    route: &SessionActivationRoute,
    request: InterruptRequest,
) -> Result<InterruptOutcome> {
    interrupt_target(js, client, route, request, None).await
}

/// Automatic stops bind the exact immutable prompt admission. A completion or
/// earlier cancellation wins; CAS retries never retarget a later invocation.
///
/// This invocation-fenced cancellation is distinct from session-level
/// `interrupt_session`, which targets the current turn without an invocation
/// identity. Worker deadline timeouts, scheduled cancellation, and orphan
/// cleanup use this fenced path to prevent late timers from canceling
/// a later independent run.
pub async fn interrupt_invocation(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    route: &SessionActivationRoute,
    request: InterruptRequest,
    prompt_seq: u64,
) -> Result<InterruptOutcome> {
    interrupt_target(
        js,
        client,
        route,
        request,
        Some(InvocationStop {
            prompt_seq,
            owning_head: None,
        }),
    )
    .await
}

/// [`interrupt_invocation`] for callers that own the invocation's live
/// admission: the stop also lands when strict steering (issue #2774) has
/// extended the invocation past its first terminator with an outstanding
/// drain. Worker deadline callbacks pass the expired invocation's id in
/// `owning_head`; the append loop re-derives ownership from the admission
/// head on every CAS attempt, so once a successor takes the head the stop
/// is scoped to the prompt's own turn and the successor's rows are never
/// cancelled.
pub async fn interrupt_invocation_stop_steered(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    route: &SessionActivationRoute,
    request: InterruptRequest,
    stop: InvocationStop,
) -> Result<InterruptOutcome> {
    interrupt_target(js, client, route, request, Some(stop)).await
}

/// Bound an admission-ownership read: a stalled broker must surface as an
/// unconfirmed error instead of hanging the cancel or silently guessing
/// ownership (issue #2774, review F1/F3).
pub(super) async fn bound_ownership_read<T>(read: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(INTERRUPT_APPEND_TIMEOUT, read)
        .await
        .context("steered-drain ownership check unconfirmed; external cleanup unknown")?
}

/// Invocation-fence targeting for a stop: the admitted prompt the stop is
/// bound to, and the invocation that must still hold the admission head for
/// the stop to reach rows past the prompt's own terminator (its steered
/// drain, issue #2774). Ownership is re-derived from the head on every CAS
/// attempt, so a successor invocation admitted mid-loop cannot have its
/// rows cancelled by a stale owner. `owning_head: None` means the stop is
/// scoped to the prompt's own turn and never touches a steered drain.
pub struct InvocationStop {
    pub prompt_seq: u64,
    pub owning_head: Option<String>,
}

/// The per-attempt view of a [`InvocationStop`]: which prompt the stop is
/// bound to, and whether that prompt's invocation still owns the steered
/// drain in this attempt's admission-head view.
#[derive(Clone, Copy)]
struct ScopedStop {
    pub prompt_seq: u64,
    pub stop_steered_drain: bool,
}

/// The message id of the user row at `prompt_seq`, if the log still has
/// one there.
fn prompt_message_id(entries: &[(u64, SessionLogEntry)], prompt_seq: u64) -> Option<String> {
    entries
        .iter()
        .find(|(seq, _)| *seq == prompt_seq)
        .and_then(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(id), role, ..
            } if role.is_user() => Some(id.clone()),
            _ => None,
        })
}

/// The `Cancel` at `seq`, when the log really has one there.
fn cancel_at(entries: &[(u64, SessionLogEntry)], seq: u64) -> bool {
    entries.iter().any(|(entry_seq, entry)| {
        *entry_seq == seq && matches!(entry, SessionLogEntry::Cancel { .. })
    })
}

/// The invocation fence verdict for the current log view: `Some` when the
/// target's state already answers the stop without another append, `None`
/// when the stop should append its `Cancel`. `stop` is this CAS attempt's
/// scoped view — its drain ownership already reflects the current
/// admission head.
fn fenced_outcome(
    entries: &[(u64, SessionLogEntry)],
    stop: ScopedStop,
) -> Result<Option<InterruptOutcome>> {
    let Some(terminal) = super::invocation_terminal_seq(entries, stop.prompt_seq) else {
        return Ok(None);
    };
    // Strict steering (issue #2774) can extend the invocation past this first
    // terminator while steered rows are outstanding. Only a caller that owns
    // the live admission may stop that extended invocation: its rows belong
    // to it. Otherwise the historical prompt is reported idle exactly as
    // before, so a stale stop can never cancel later work.
    let steered_live =
        stop.stop_steered_drain && super::steered_rows_outstanding(entries, stop.prompt_seq)?;
    if !steered_live {
        return Ok(Some(if cancel_at(entries, terminal) {
            InterruptOutcome::AlreadyInterrupted {
                cancel_seq: terminal,
            }
        } else {
            InterruptOutcome::Idle
        }));
    }
    // The drain keeps the invocation live: the stop appends below, unless the
    // extended end is already a Cancel.
    let boundary = super::steered_terminal_seq(entries, stop.prompt_seq).unwrap_or(terminal);
    if cancel_at(entries, boundary) {
        return Ok(Some(InterruptOutcome::AlreadyInterrupted {
            cancel_seq: boundary,
        }));
    }
    Ok(None)
}

/// The `Cancel` carrying our own cancellation id among a conflict's newer
/// entries: our append landed before, and the lost ack is its confirmation.
fn own_cancel_seq(entries: &[(u64, SessionLogEntry)], cancellation_id: &str) -> Option<u64> {
    entries.iter().find_map(|(seq, entry)| match entry {
        SessionLogEntry::Cancel {
            cancellation_id: Some(id),
            ..
        } if id == cancellation_id => Some(*seq),
        _ => None,
    })
}

async fn interrupt_target(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    route: &SessionActivationRoute,
    request: InterruptRequest,
    stop: Option<InvocationStop>,
) -> Result<InterruptOutcome> {
    let log = NatsSessionLog::new(js.clone(), request.session_id.clone());
    // Steered-drain ownership is re-derived from the admission head on every
    // CAS attempt: the log alone cannot tell a successor invocation's rows
    // from this prompt's steered ones once the successor's prompt row is
    // appended, so the head is the authority that keeps a stale owner's
    // Cancel off the successor's work.
    let head_store = steered_stop_store(js, stop.as_ref(), request.replicas).await?;
    let mut entries: Vec<_> = match &stop {
        Some(_) => log.load_events_latest_async().await?,
        None => log.last_entry_async().await?.into_iter().collect(),
    };
    for _ in 0..MAX_CAS_ATTEMPTS {
        let scoped = match stop.as_ref() {
            Some(stop) => Some(scoped_stop(stop, head_store.as_ref(), &request.session_id).await?),
            None => None,
        };
        let settled = match scoped {
            Some(scoped) => fenced_outcome(&entries, scoped)?,
            None => decide(&entries),
        };
        if let Some(outcome) = settled {
            return Ok(outcome);
        }
        let tail = entries
            .last()
            .map_or(scoped.map_or(0, |scoped| scoped.prompt_seq), |(seq, _)| {
                *seq
            });
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
                announce(
                    js,
                    client,
                    route,
                    Accepted::new(&request, cancel_seq),
                    stop.is_none(),
                );
                return Ok(InterruptOutcome::Accepted { cancel_seq });
            }
            FencedAppend::Conflict { entries: newer } => {
                // Our own lost ack shows up as a Cancel carrying our id.
                if let Some(cancel_seq) = own_cancel_seq(&newer, &request.cancellation_id) {
                    announce(
                        js,
                        client,
                        route,
                        Accepted::new(&request, cancel_seq),
                        stop.is_none(),
                    );
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

/// The metadata store for re-deriving steered-drain ownership in the stop
/// loop, created only when the stop claims an owning invocation at all
/// (issue #2774).
async fn steered_stop_store(
    js: &async_nats::jetstream::Context,
    stop: Option<&InvocationStop>,
    replicas: usize,
) -> Result<Option<crate::nats_session_metadata::SessionMetadataStore>> {
    match stop.filter(|stop| stop.owning_head.is_some()) {
        Some(_) => Ok(Some(
            crate::nats_session_metadata::SessionMetadataStore::ensure(js, replicas).await?,
        )),
        None => Ok(None),
    }
}

/// This CAS attempt's scoped stop: the recorded owning invocation re-derived
/// against the current admission head, so a stale owner — a successor
/// admitted mid-loop — cannot reach the steered rows. No store means no
/// owning invocation was recorded, and the stop stays scoped to the
/// prompt's own turn.
async fn scoped_stop(
    stop: &InvocationStop,
    store: Option<&crate::nats_session_metadata::SessionMetadataStore>,
    session_id: &str,
) -> Result<ScopedStop> {
    let stop_steered_drain = match store {
        Some(store) => {
            let head = store.admission_head(session_id).await?;
            stop.owning_head
                .as_deref()
                .is_some_and(|owning| head.as_deref() == Some(owning))
        }
        None => false,
    };
    Ok(ScopedStop {
        prompt_seq: stop.prompt_seq,
        stop_steered_drain,
    })
}

/// `None` means "append a Cancel now".
///
/// `window` is the log's tail, not the log: the last entry, plus whatever a
/// conflict appended after it. Without a terminator in view the request
/// cannot see the turn's start and treats the turn as in progress. An idle
/// session whose last entry is a mutation or control entry therefore gets a
/// stray `Cancel`, which the protocol tolerates; reading further back to
/// avoid it would put the whole transcript on the interrupt's path again.
pub(super) fn decide(window: &[(u64, SessionLogEntry)]) -> Option<InterruptOutcome> {
    if window.is_empty() {
        return Some(InterruptOutcome::Idle);
    }
    let turn = current_turn_entries(window);
    if turn.len() == window.len() {
        return None;
    }
    let has_user = turn
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Message { role, .. } if role.is_user()));
    if has_user {
        return None;
    }
    if last_terminator_is_cancel(window) {
        let cancel_seq = window
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
    replicas: usize,
    cancellation_id: String,
    cancel_seq: u64,
}

impl Accepted {
    fn new(request: &InterruptRequest, cancel_seq: u64) -> Self {
        Self {
            session_id: request.session_id.clone(),
            cluster: request.cluster.clone(),
            replicas: request.replicas,
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
    session_hint: bool,
) {
    let js = js.clone();
    let client = client.clone();
    let route = route.clone();
    tokio::spawn(async move {
        let hint = ControlCommand::Interrupt {
            cancellation_id: accepted.cancellation_id.clone(),
        };
        if session_hint {
            if let Err(error) = publish_control_command(&client, &accepted.session_id, &hint).await
            {
                log::debug!("interrupt hint not published: {error:#}");
            }
        }
        let result = match &route {
            SessionActivationRoute::ClusterShared => {
                let activation = SessionActivate::new(&accepted.session_id)
                    .with_requested_seq(accepted.cancel_seq);
                publish_session_activate(&js, &accepted.cluster, &activation, accepted.replicas)
                    .await
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
