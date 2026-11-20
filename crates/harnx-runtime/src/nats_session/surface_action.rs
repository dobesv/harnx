//! Surface action client API for NatsSession (plan T5.1).
//!
//! `submit_surface_action` routes the submission to the lease-holding worker
//! over the control subject, which validates against the fresh log, runs a
//! declared direct tool target through the operator RPC once per accepted
//! action, appends the fenced first-wins record and pushes `note` visibility
//! into the running turn. When the log already carries the accepted record —
//! a lost or late reply, a racing client — the retry classifies from the log
//! and recovers the `turn`-visibility effect from the record instead of
//! dropping it (review C7). When no worker answers and no lease may be held,
//! the client falls back to validating and recording itself (unfenced CAS,
//! like a frontend `Cancel`), and the next turn's fold delivers `note`
//! records. An unknown lease state never takes that fallback: fail closed
//! (C8) — no unfenced write while a lease may be held by a worker that did
//! not answer.

use super::{
    NatsSession, CONTROL_ACK_ATTEMPT_TIMEOUT, CONTROL_ACK_RETRY_DELAY, CONTROL_RECOVERY_TIMEOUT,
};
use crate::nats_session_log::{FencedAppend, NatsSessionLog};
use crate::nats_worker::{control_subject, ControlCommand};
use crate::surfaces::{self, SurfaceActionOutcome, SurfaceActionReply, SurfaceActionSubmission};
use anyhow::Result;
use futures_util::StreamExt;
use harnx_core::interaction::non_modal::SurfaceVisibility;
use harnx_core::session::SessionLogEntry;
use std::time::Duration;

/// How long the client waits for the worker's final outcome after its early
/// acknowledgement. Direct tool targets run through the operator RPC, which
/// the worker caps at 600 s, plus append and reply margin.
const ACKED_OUTCOME_TIMEOUT: Duration = Duration::from_secs(660);
/// Wait slice between record checks while an acknowledged attempt runs.
const ACKED_WAIT_SLICE: Duration = Duration::from_secs(5);

impl NatsSession {
    /// Submit a user action on a published non-modal A2UI surface.
    ///
    /// Rejections (`unknown_surface`, `undeclared_action`, `invalid_visibility`,
    /// `invalid_data`, `unauthorized_tool`, `conflicting`) never append, execute
    /// or dispatch anything. `turn` visibility enqueues the action's user
    /// message after the record lands, which starts a turn when the session is
    /// idle and joins the running one otherwise — the same path `enqueue_text`
    /// uses — including when the reply was lost and only the record remains.
    /// `note` visibility reaches the next model call without a new turn.
    ///
    /// At-most-once execution (review B1): the lease-holding worker
    /// acknowledges the command before running anything, and while that
    /// acknowledgement is outstanding the client never executes the target
    /// itself. The idle fallback (unfenced append, like a frontend `Cancel`)
    /// is reached only when no worker ever acknowledged within the recovery
    /// budget and the session lease is known to be unheld (C8).
    pub async fn submit_surface_action(
        &self,
        submission: &SurfaceActionSubmission,
    ) -> Result<SurfaceActionOutcome> {
        let command = ControlCommand::SurfaceAction {
            submission: submission.clone(),
        };
        let payload: bytes::Bytes = command.to_bytes()?.into();
        let subject = control_subject(&self.storage_key);
        // One inbox collects the worker's early acknowledgement and its final
        // outcome; the subscription stays open for the whole call so a slow
        // outcome is never mistaken for a lost one.
        let inbox = self.client.new_inbox();
        let mut replies = self.client.subscribe(inbox.clone()).await?;
        let started = tokio::time::Instant::now();
        let recovery_deadline = started + surface_action_recovery_timeout();
        let acked_deadline = started + ACKED_OUTCOME_TIMEOUT;
        let mut acked = false;
        loop {
            if !acked {
                // Unacknowledged resends are safe: the worker coalesces
                // handlers per surface generation, so duplicates classify
                // from the durable record instead of re-executing.
                self.client
                    .publish_with_reply(subject.clone(), inbox.clone(), payload.clone())
                    .await?;
            }
            // Drain whatever arrives within one ack window. The
            // acknowledgement only marks the worker as owning the attempt;
            // any other outcome is final.
            if let Some(answer) = drain_ack_window(&mut replies, &mut acked).await {
                return self.finish_surface_action(answer).await;
            }

            if let Some(outcome) = self.recorded_outcome(submission).await? {
                return Ok(outcome);
            }

            let now = tokio::time::Instant::now();
            if acked {
                // A worker owns the attempt; wait it out. The periodic record
                // check recovers a worker that appended and died before its
                // final reply.
                if now >= acked_deadline {
                    anyhow::bail!(
                        "surface action abandoned: the worker acknowledged \
                         the command but never delivered an outcome"
                    );
                }
                tokio::time::sleep(ACKED_WAIT_SLICE).await;
                continue;
            }
            if now >= recovery_deadline {
                return self.recover_or_idle(submission).await;
            }
            tokio::time::sleep(CONTROL_ACK_RETRY_DELAY).await;
        }
    }

    /// The outcome implied by an already-recorded action, if the log carries
    /// one for this generation — a racing client, or a reply lost after a
    /// successful apply (review C7): classify from the log and recover the
    /// `turn` effect instead of resubmitting a recorded action. A matching
    /// `turn` record delivers its turn text on the way through.
    async fn recorded_outcome(
        &self,
        submission: &SurfaceActionSubmission,
    ) -> Result<Option<SurfaceActionOutcome>> {
        let entries = self.load_durable_entries().await?;
        let accepted = match surfaces::existing_surface_action(
            &entries,
            &submission.surface_id,
            submission.publish_seq,
        ) {
            Some(accepted) => accepted,
            None => return Ok(None),
        };
        self.recover_accepted_action(&accepted, submission).await?;
        Ok(Some(if accepted.matches(submission) {
            SurfaceActionOutcome::AlreadyApplied
        } else {
            SurfaceActionOutcome::Conflicting
        }))
    }

    /// The end of the recovery budget with no acknowledgement: only a known
    /// lease-free state may take the idle fallback (an unfenced append, like
    /// a frontend `Cancel`). A held lease means a worker owns the session —
    /// a mixed-version worker that drops the unknown command — and an
    /// unknown lease state fails closed (C8): never write unfenced while a
    /// lease may be held.
    async fn recover_or_idle(
        &self,
        submission: &SurfaceActionSubmission,
    ) -> Result<SurfaceActionOutcome> {
        match self.session_lease_is_held().await {
            Ok(false) => self.submit_surface_action_idle(submission).await,
            Ok(true) => Err(anyhow::anyhow!(
                "surface action abandoned: a worker holds the session \
                 lease but the control plane never answered"
            )),
            Err(error) => Err(anyhow::anyhow!(
                "surface action abandoned: session lease state \
                 could not be read: {error:#}"
            )),
        }
    }

    /// Whether some worker currently holds this session's lease. A missing
    /// lease bucket means no lease can exist; any other lookup failure is an
    /// error so the caller fails closed instead of writing unfenced (C8).
    ///
    /// The probe is advisory, not an ownership signal: it has been observed
    /// returning false while a worker was mid-execution holding its lease
    /// in memory, so it must never decide whether a worker owns a
    /// submission — only the worker's `Pending` acknowledgement is an
    /// ownership proof (review R1). Here it only gates the recovery-deadline
    /// idle fallback, which is reached after no worker acknowledged for the
    /// whole recovery budget.
    async fn session_lease_is_held(&self) -> Result<bool> {
        crate::nats_lease::session_has_active_lease(&self.jetstream, &self.storage_key).await
    }

    /// Finish an accepted action on the worker's reply: enqueue the turn
    /// text for `turn` visibility, exactly once even if a concurrent
    /// submission already recovered it from the durable record.
    async fn finish_surface_action(
        &self,
        answer: SurfaceActionReply,
    ) -> Result<SurfaceActionOutcome> {
        // `already_applied` carries the turn text too: a retry whose
        // original reply was lost recovers the `turn` effect from the
        // record (review C7).
        if answer.outcome.is_applied() {
            if let Some(turn_text) = &answer.turn_text {
                self.deliver_turn_text_once(turn_text).await?;
            }
        }
        Ok(answer.outcome)
    }

    /// Classify an already-recorded action on behalf of a retry whose original
    /// reply was lost or late (C7): a `turn`-visibility record delivers its
    /// turn text even though the reply that carried it never arrived.
    ///
    /// First-wins identity is `(action, data)`: a conflicting submit never
    /// recovers an effect, and an already-delivered turn is not delivered
    /// twice — the log check in [`deliver_turn_text_once`] dedupes against a
    /// racing client that read the reply.
    async fn recover_accepted_action(
        &self,
        accepted: &surfaces::AcceptedSurfaceAction,
        submission: &SurfaceActionSubmission,
    ) -> Result<()> {
        if accepted.matches(submission) && accepted.is_turn() {
            if let Some(text) = accepted.note_text.as_deref() {
                self.deliver_turn_text_once(text).await?;
            }
        }
        Ok(())
    }

    /// Enqueue the turn text for `turn` visibility unless its user row is
    /// already durable. Concurrent submissions of the same accepted action
    /// (one got the reply, one recovered from the record) both route through
    /// here, so the turn starts or joins exactly once.
    async fn deliver_turn_text_once(&self, turn_text: &str) -> Result<()> {
        if turn_text.trim().is_empty() {
            return Ok(());
        }
        let entries = self.load_durable_entries().await?;
        let already_delivered = entries
            .iter()
            .any(|(_, entry)| self_turn_row(entry, turn_text));
        if already_delivered {
            return Ok(());
        }
        // The action came from the user's frontend, so the turn text it
        // produced carries the same external admission a typed prompt from
        // that frontend would.
        self.clone()
            .with_external_admission()
            .enqueue_text(turn_text)
            .await?;
        Ok(())
    }

    /// Idle-session fallback: no worker answered the control subject and no
    /// lease may be held, so the client validates from the log, runs a
    /// declared tool target through the operator RPC once, and appends the
    /// record itself with an unfenced CAS — the same trust a frontend
    /// `Cancel` has. `note` records are delivered by the next turn's fold;
    /// `turn` enqueues here as in the live path.
    ///
    /// The tool summary is computed once, outside the CAS retry loop (B1):
    /// a conflict from an unrelated append only re-validates and re-appends.
    async fn submit_surface_action_idle(
        &self,
        submission: &SurfaceActionSubmission,
    ) -> Result<SurfaceActionOutcome> {
        let mut tool_summary: Option<String> = None;
        for _ in 0..surfaces::append_conflict_rounds() {
            let entries = self.load_durable_entries().await?;
            let declaration = match surfaces::validate_surface_submission(&entries, submission) {
                Ok(declaration) => declaration,
                Err(outcome) => return Ok(outcome),
            };
            match &declaration.tool {
                Some(target) if tool_summary.is_none() => {
                    // The tool runs once, outside the CAS loop (B1): a
                    // conflict from an unrelated append re-appends the same
                    // summary instead of re-running the target.
                    tool_summary = match self.run_idle_tool(submission, target).await {
                        Ok(summary) => Some(summary),
                        Err(outcome) => return Ok(outcome),
                    };
                }
                _ => {}
            }
            let text = harnx_core::interaction::non_modal::action_text(
                &submission.response.action,
                &submission.response.data,
                submission.user_message.as_deref(),
                tool_summary.as_deref(),
            );
            let entry = idle_record_entry(submission, &declaration, &text);
            let log = NatsSessionLog::new_with_replicas(
                self.jetstream.clone(),
                self.storage_key.clone(),
                self.attachment_replicas,
            );
            let expected_tail = entries.last().map_or(0, |(seq, _)| *seq);
            match log
                .append_fenced(&entry, expected_tail, &uuid::Uuid::new_v4().to_string())
                .await?
            {
                FencedAppend::Appended(_) => {
                    if declaration.visibility == SurfaceVisibility::Turn {
                        self.deliver_turn_text_once(&text).await?;
                    }
                    return Ok(SurfaceActionOutcome::Applied);
                }
                // The tail moved: reload and revalidate — a racing client
                // may have recorded this action first. The tool never
                // re-runs; its summary survives the loop.
                FencedAppend::Conflict { .. } => {
                    tokio::time::sleep(surfaces::APPEND_CONFLICT_RETRY_DELAY).await;
                    continue;
                }
            }
        }
        anyhow::bail!("surface action append lost every CAS race; nothing was recorded");
    }

    /// Run the declared tool target through the operator RPC once. `Err`
    /// carries the outcome the whole attempt reports: an unauthorized or
    /// failed target ends the attempt without recording anything.
    async fn run_idle_tool(
        &self,
        submission: &SurfaceActionSubmission,
        target: &harnx_core::interaction::non_modal::SurfaceToolTarget,
    ) -> std::result::Result<String, SurfaceActionOutcome> {
        let operator_subject = match crate::operator_tools::subject(
            &self.config.cluster,
            &self.config.activation_route,
        ) {
            Ok(subject) => subject,
            Err(error) => {
                log::warn!("surface action operator subject failed: {error:#}");
                return Err(SurfaceActionOutcome::ToolFailed);
            }
        };
        let route = surfaces::SurfaceToolRoute {
            client: &self.client,
            operator_subject: &operator_subject,
            session_key: &self.storage_key,
            abort_signal: &self.abort_signal,
        };
        surfaces::run_surface_tool(&route, target, &submission.response.data).await
    }
}

/// Consume the inbox for one ack window (review B1). `Pending` marks the
/// worker as owning the attempt — sets `acked` and keeps draining — while any
/// other reply is final and is returned. Malformed payloads are skipped.
async fn drain_ack_window(
    replies: &mut async_nats::Subscriber,
    acked: &mut bool,
) -> Option<SurfaceActionReply> {
    while let Ok(Some(message)) =
        tokio::time::timeout(CONTROL_ACK_ATTEMPT_TIMEOUT, replies.next()).await
    {
        let Ok(answer) = serde_json::from_slice::<SurfaceActionReply>(&message.payload) else {
            continue;
        };
        match answer.outcome {
            SurfaceActionOutcome::Pending => *acked = true,
            // A non-holder's prompt refusal (review N-B): before any
            // acknowledgement it is a redirect hint, not an outcome — this
            // worker never claimed the attempt, so keep resending until the
            // actual holder acknowledges or the recovery budget runs out.
            // After the ack the refusal is final: the owning worker lost
            // the attempt with nothing recorded, and a client-driven
            // resend could re-run a non-idempotent tool under the new
            // holder, so the client returns instead of retrying itself.
            SurfaceActionOutcome::LeaseLost if !*acked => {}
            _ => return Some(answer),
        }
    }
    None
}

/// The record entry for an idle attempt. The model text carries the delivered
/// note/turn text; `invisible` stays text-free by contract.
fn idle_record_entry(
    submission: &SurfaceActionSubmission,
    declaration: &harnx_core::interaction::non_modal::SurfaceActionDeclaration,
    text: &str,
) -> SessionLogEntry {
    let model_text = match declaration.visibility {
        SurfaceVisibility::Invisible => None,
        SurfaceVisibility::Note | SurfaceVisibility::Turn => Some(text.to_string()),
    };
    surfaces::surface_action_entry(submission, declaration, model_text.as_deref(), 0)
}

/// A genuine user row carrying exactly this turn text — the signature that a
/// `turn`-visibility action already delivered. Runtime-authored rows (hook
/// context, timing) are context, not user prompts.
fn self_turn_row(entry: &SessionLogEntry, turn_text: &str) -> bool {
    let SessionLogEntry::Message {
        id, role, content, ..
    } = entry
    else {
        return false;
    };
    if role != &harnx_core::message::MessageRole::User {
        return false;
    }
    let runtime_authored = harnx_core::runtime_note::is_runtime_context_id(id.as_deref())
        || id.as_deref().is_some_and(|id| {
            id.starts_with(harnx_core::timing_identity::TIMING_MESSAGE_ID_PREFIX)
        });
    !runtime_authored && content.to_text() == turn_text
}

/// The control-plane recovery budget before a surface action gives up on the
/// lease holder. The `HARNX_SURFACE_ACTION_RECOVERY_TIMEOUT_MS` override is a
/// test knob; production keeps [`CONTROL_RECOVERY_TIMEOUT`].
fn surface_action_recovery_timeout() -> std::time::Duration {
    std::env::var("HARNX_SURFACE_ACTION_RECOVERY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|millis| *millis > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(CONTROL_RECOVERY_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_action_command_round_trips_through_the_control_payload() {
        let command = ControlCommand::SurfaceAction {
            submission: SurfaceActionSubmission {
                surface_id: "s-1".into(),
                publish_seq: 7,
                response: harnx_core::interaction::InteractionResponse {
                    action: "deploy".into(),
                    data: serde_json::from_value(serde_json::json!({"env": "prod"})).unwrap(),
                    note: None,
                },
                user_message: Some("deploy now".into()),
            },
        };
        let bytes = command.to_bytes().unwrap();
        let decoded = ControlCommand::from_bytes(&bytes).unwrap();
        let ControlCommand::SurfaceAction { submission } = decoded else {
            panic!("expected surface action command");
        };
        assert_eq!(submission.surface_id, "s-1");
        assert_eq!(submission.publish_seq, 7);
        assert_eq!(submission.response.action, "deploy");
        assert_eq!(submission.user_message.as_deref(), Some("deploy now"));
    }

    #[test]
    fn surface_action_reply_decodes_rejections_without_turn_text() {
        let reply: SurfaceActionReply =
            serde_json::from_str("{\"outcome\":\"undeclared_action\"}").unwrap();
        assert_eq!(reply.outcome, SurfaceActionOutcome::UndeclaredAction);
        assert!(reply.turn_text.is_none());
        let reply: SurfaceActionReply =
            serde_json::from_str("{\"outcome\":\"applied\",\"turn_text\":\"go\"}").unwrap();
        assert_eq!(reply.outcome, SurfaceActionOutcome::Applied);
        assert_eq!(reply.turn_text.as_deref(), Some("go"));
    }
}
