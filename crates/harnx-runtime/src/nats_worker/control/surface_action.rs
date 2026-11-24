//! Surface action control handler (plan T5.1).
//!
//! The lease-holding worker re-validates the submission against its fresh log,
//! runs a declared direct tool target through the operator RPC (so the
//! agent's tool permissions and middleware apply, exactly like an operator
//! `.call`), then appends the fenced first-wins record. `note` visibility is
//! pushed into the running turn's pending-context queue; the client learns the
//! outcome — and the text to enqueue for `turn` visibility — from the reply.
//!
//! At-most-once execution (review B1): handlers for the same surface
//! generation are serialized on a per-`(surface_id, publish_seq)` gate, so
//! client resends wait for the in-flight attempt instead of re-validating a
//! not-yet-recorded action, and the tool runs once per accepted action,
//! outside the CAS retry loop — a conflict from an unrelated append only
//! re-validates and re-appends, never re-executes. Every terminal state
//! answers — CAS exhaustion (`append_failed`), lease loss, and a
//! contained panic (the ack-time ownership check in
//! [`super::surface_command`] keeps unrelated workers from answering at
//! all) — so the client never resends into silence.

use super::{SessionControlHandler, SurfaceActionGateMap};
use crate::surfaces::{self, SurfaceActionOutcome, SurfaceActionReply, SurfaceActionSubmission};

use harnx_core::interaction::non_modal::{self, SurfaceActionDeclaration, SurfaceVisibility};
use harnx_core::session::SessionLogEntry;
use std::sync::Arc;

impl SessionControlHandler {
    pub(super) async fn apply_surface_action(
        &self,
        submission: &SurfaceActionSubmission,
        reply: Option<async_nats::Subject>,
    ) {
        // Serialize handlers for the same surface generation. A client that
        // resends while the first attempt is still in flight (500 ms ack
        // timeout vs. a tool that runs for minutes) queues here and then
        // re-validates against the fresh log: once the first attempt's
        // record lands, every queued resend classifies as already applied
        // without running anything.
        let gate = self.surface_action_gate(submission);
        let _guard = gate.lock().await;
        let answer = self.accept_surface_action(submission).await;
        self.publish_side_effects(reply, &answer).await;
    }

    /// The reply-side effects of a handled surface action: refresh live
    /// listeners for an applied action, and answer the client. Lease loss
    /// answers too (review N-B): only a worker that proved ownership before
    /// its early ack reaches here, so its `LeaseLost` means this attempt
    /// died with nothing recorded — silence would stall the client for its
    /// whole acked-outcome budget. The client treats the refusal as final
    /// after the ack and never resends on its own, so at-most-once is
    /// preserved.
    async fn publish_side_effects(
        &self,
        reply: Option<async_nats::Subject>,
        answer: &SurfaceActionReply,
    ) {
        if answer.outcome.is_applied() {
            self.publish_updated().await;
        }
        self.publish_control_reply(reply, answer, "surface action")
            .await;
    }

    async fn accept_surface_action(
        &self,
        submission: &SurfaceActionSubmission,
    ) -> SurfaceActionReply {
        // The tool summary computed by the single execution: kept outside the
        // append loop so a CAS conflict from an unrelated append re-uses it
        // instead of re-running a non-idempotent target.
        let mut tool_summary: Option<String> = None;
        for _ in 0..surfaces::append_conflict_rounds() {
            let entries = match self.load_entries_or_reject().await {
                Ok(entries) => entries,
                Err(reply) => return reply,
            };
            let declaration = match surfaces::validate_surface_submission(&entries, submission) {
                Ok(declaration) => declaration,
                // Rejections reach the client unchanged; nothing ran or
                // appended. An `already_applied` retry whose original
                // reply was lost still carries the `turn` effect (review
                // C7): the record is the authority, not the reply.
                Err(outcome) => return classification_reply(&entries, submission, outcome),
            };
            if let Some(reply) = self
                .run_target_once(submission, &declaration, &mut tool_summary)
                .await
            {
                return reply;
            }
            let text = non_modal::action_text(
                &submission.response.action,
                &submission.response.data,
                submission.user_message.as_deref(),
                tool_summary.as_deref(),
            );
            // The record carries its delivered text for `note` and `turn` —
            // the fold reads it back for `note`, and a retry that sees only
            // the record (lost reply, review C7) recovers the `turn` effect
            // from it. `invisible` stays text-free by contract.
            let entry = self.record_entry(submission, &declaration, &text);
            let appended = match self.append_record(&entries, &entry).await {
                Ok(appended) => appended,
                Err(reply) => return reply,
            };
            if appended {
                return self.applied_reply(&declaration, text).await;
            }
            // Lost the CAS race to an unrelated append (or a racing client
            // that recorded first): reload, back off, and re-validate. The
            // tool never re-runs — `tool_summary` survives the loop.
            tokio::time::sleep(surfaces::APPEND_CONFLICT_RETRY_DELAY).await;
        }
        // Explicit failure instead of silence (review B1): the client learns
        // the action was not recorded instead of resending into a listener
        // that never answers.
        log::warn!(
            "surface action '{}' on surface '{}' lost every CAS race",
            submission.response.action,
            submission.surface_id
        );
        SurfaceActionReply::rejected(SurfaceActionOutcome::AppendFailed)
    }

    /// The fresh log tail, or the reply to send when it cannot be loaded.
    /// The lease is re-validated first: a handler that lost ownership must
    /// not append under a stale fence.
    async fn load_entries_or_reject(
        &self,
    ) -> std::result::Result<Vec<(u64, SessionLogEntry)>, SurfaceActionReply> {
        if !self.holds_session_lease().await {
            return Err(SurfaceActionReply::rejected(
                SurfaceActionOutcome::LeaseLost,
            ));
        }
        self.backend
            .load_events_latest_async()
            .await
            .map_err(|error| {
                log::warn!("surface action log load failed: {error:#}");
                SurfaceActionReply::rejected(SurfaceActionOutcome::AppendFailed)
            })
    }

    /// Execute-then-record (v1): the tool runs before the record, so its
    /// result can be quoted in the note. A crash in between leaves no record,
    /// so a user retry would re-run the tool — documented. Within one
    /// accepted action the summary is computed exactly once, outside the CAS
    /// loop, so conflicts re-append instead of re-running. Returns the reply
    /// to send when execution must not proceed past this round.
    async fn run_target_once(
        &self,
        submission: &SurfaceActionSubmission,
        declaration: &SurfaceActionDeclaration,
        tool_summary: &mut Option<String>,
    ) -> Option<SurfaceActionReply> {
        let target = match &declaration.tool {
            Some(target) if tool_summary.is_none() => target,
            _ => return None,
        };
        let route = surfaces::SurfaceToolRoute {
            client: &self.client,
            operator_subject: &self.operator_subject,
            session_key: &self.session_id,
            abort_signal: &self.abort_signal,
        };
        match surfaces::run_surface_tool(&route, target, &submission.response.data).await {
            Ok(summary) => {
                *tool_summary = Some(summary);
                None
            }
            Err(outcome) => Some(SurfaceActionReply::rejected(outcome)),
        }
    }

    /// The fenced record for this round's attempt. The model text carries
    /// the delivered note/turn text; `invisible` stays text-free.
    fn record_entry(
        &self,
        submission: &SurfaceActionSubmission,
        declaration: &SurfaceActionDeclaration,
        text: &str,
    ) -> SessionLogEntry {
        let model_text = match declaration.visibility {
            SurfaceVisibility::Invisible => None,
            SurfaceVisibility::Note | SurfaceVisibility::Turn => Some(text.to_string()),
        };
        surfaces::surface_action_entry(
            submission,
            declaration,
            model_text.as_deref(),
            self.lease.fence_token(),
        )
    }

    /// Append the record against the round's tail, fenced by the lease.
    /// Returns whether the append landed; the reply on an append failure
    /// is explicit so the client does not resend into silence (B1).
    async fn append_record(
        &self,
        entries: &[(u64, SessionLogEntry)],
        entry: &SessionLogEntry,
    ) -> std::result::Result<bool, SurfaceActionReply> {
        let expected_tail = entries.last().map_or(0, |(seq, _)| *seq);
        self.backend
            .append_event_with_expected_last_sequence_and_lease(
                entry,
                expected_tail,
                self.lease.as_ref(),
            )
            .await
            .map(|appended| appended.is_some())
            .map_err(|error| {
                log::warn!("surface action append failed: {error:#}");
                SurfaceActionReply::rejected(SurfaceActionOutcome::AppendFailed)
            })
    }

    /// The applied reply. `note` is the only visibility with a mid-turn
    /// effect here: `invisible` is durable-only and `turn` text is
    /// enqueued by the client from the reply, so neither may be pushed
    /// into the model's context queue (review B2).
    async fn applied_reply(
        &self,
        declaration: &SurfaceActionDeclaration,
        text: String,
    ) -> SurfaceActionReply {
        if declaration.visibility == SurfaceVisibility::Note {
            self.surface_actions.push_note(&text).await;
        }
        SurfaceActionReply {
            outcome: SurfaceActionOutcome::Applied,
            turn_text: (declaration.visibility == SurfaceVisibility::Turn).then_some(text),
        }
    }
}

/// The reply for a classified outcome. An `already_applied` retry whose
/// original reply was lost still carries the `turn` effect from the durable
/// record (review C7); every other classification is text-free.
fn classification_reply(
    entries: &[(u64, SessionLogEntry)],
    submission: &SurfaceActionSubmission,
    outcome: SurfaceActionOutcome,
) -> SurfaceActionReply {
    let turn_text = if outcome == SurfaceActionOutcome::AlreadyApplied {
        surfaces::existing_surface_action(entries, &submission.surface_id, submission.publish_seq)
            .filter(|accepted| accepted.is_turn())
            .and_then(|accepted| accepted.note_text)
    } else {
        None
    };
    SurfaceActionReply { outcome, turn_text }
}

/// Live in-flight gate for a surface generation: upgrade the weak entry if
/// its handler is still running, otherwise install a fresh one. Dead entries
/// are swept on the way through.
pub(super) fn live_gate(
    gates: &mut SurfaceActionGateMap,
    key: (String, u64),
) -> Arc<tokio::sync::Mutex<()>> {
    gates.retain(|_, gate| gate.strong_count() > 0);
    upgrade_live(gates, &key).unwrap_or_else(|| install_gate(gates, key))
}

/// The generation's gate if its handler is still running.
fn upgrade_live(
    gates: &SurfaceActionGateMap,
    key: &(String, u64),
) -> Option<Arc<tokio::sync::Mutex<()>>> {
    gates.get(key).and_then(std::sync::Weak::upgrade)
}

/// A fresh gate for a generation with nothing in flight: later lookups join
/// it until its last handler drops it and the next sweep removes the entry.
fn install_gate(
    gates: &mut SurfaceActionGateMap,
    key: (String, u64),
) -> Arc<tokio::sync::Mutex<()>> {
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}
