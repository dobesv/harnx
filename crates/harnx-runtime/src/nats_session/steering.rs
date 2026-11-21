//! Steering joins a prompt to a session's active turn instead of reserving a
//! new invocation (issue #2672). This module holds the decisions
//! `admit_prompt` delegates to: the replay of an already-admitted prompt
//! (with recovery of a join whose turn provably ended undelivered, review
//! F1/M-A) and the fresh steering-join decision (which never joins a
//! fixed-admission head, review F2).
use super::dispatch::user_message_present;
use super::prompt_append::LateSteeringRefusal;
use super::{new_client_message_id, AppendedPrompt, NatsSession};
use crate::nats_session_metadata::{AdmissionAuthority, InvocationAdmission, VoidJoinRelease};
use anyhow::{Context, Result};
use harnx_core::message::MessageContent;
use harnx_core::session::SessionLogEntry;

impl NatsSession {
    /// Decide the early return for an already-admitted prompt. A steering
    /// join whose bound turn provably ended before this prompt's message was
    /// appended can never deliver — the append backstop refuses terminal
    /// invocations — so it is recovered by re-deciding instead of handing
    /// back a dead invocation (issue #2672, review F1/M-A). External steering
    /// keeps today's behavior.
    pub(super) async fn replay_prompt_admission(
        &self,
        user_msg_id: &str,
        saved: &InvocationAdmission,
        content: Option<&MessageContent>,
    ) -> Result<String> {
        if saved.invocation_id.as_str() != user_msg_id
            && matches!(
                self.admission_authority.as_ref(),
                Some(AdmissionAuthority::Inherited { .. })
            )
        {
            if let Some(target) = self
                .recover_undelivered_steering(user_msg_id, saved, content)
                .await?
            {
                return Ok(target);
            }
        }
        Ok(saved.invocation_id.as_str().to_owned())
    }

    /// Recover a steering join whose bound turn provably ended before this
    /// prompt's message was appended (issue #2672, review F1/M-A). The prompt
    /// id is re-decided — join the live non-fixed turn, else reserve this
    /// prompt's own invocation — and the binding is CAS-moved off the dead
    /// invocation only while it still names it, so a concurrent retry's
    /// redirect is never clobbered.
    ///
    /// Returns `Some(invocation)` to deliver under (the redirected target, or a
    /// concurrent retry's target that is equally valid for the same call), or
    /// `None` when the bound turn is still live or the message was already
    /// appended, and today's replay behavior applies.
    async fn recover_undelivered_steering(
        &self,
        user_msg_id: &str,
        saved: &InvocationAdmission,
        content: Option<&MessageContent>,
    ) -> Result<Option<String>> {
        let entries = self.load_durable_entries().await?;
        // The delivered row returns first, matching dispatch's
        // `finish_joined_dispatch` (review round 2, C10): a delivered prompt
        // replays as-is, before any recovery decision.
        if user_message_present(&entries, user_msg_id) {
            return Ok(None);
        }
        if !self
            .metadata_store
            .admission_is_terminal(&self.storage_key, saved.invocation_id.as_str(), &entries)
            .await?
        {
            return Ok(None);
        }
        anyhow::ensure!(
            !super::fixed_admission::admission_closed(&entries, user_msg_id, user_msg_id),
            "fixed admission closed without prompt; cannot replay through ordinary admission"
        );
        let authority = self.admission_authority.as_ref().context(
            "execution admission requires explicit frontend authority or inherited run context",
        )?;
        let mut intent = InvocationAdmission::new(
            authority,
            user_msg_id.to_owned(),
            self.admission_timeout,
            self.admission_token_budget,
        );
        intent.parent_storage_key = self.parent_session_id.clone();
        intent.prompt_content = content.cloned();
        let target = match self
            .metadata_store
            .active_admission(&self.storage_key, &entries)
            .await?
        {
            Some(active) if active.fixed_ticket.is_none() => {
                active.invocation_id.as_str().to_owned()
            }
            _ => {
                let reserved = self
                    .metadata_store
                    .reserve_admission(&self.storage_key, &intent, &entries)
                    .await?;
                reserved.invocation_id.as_str().to_owned()
            }
        };
        let bound = self
            .metadata_store
            .redirect_prompt_admission(VoidJoinRelease {
                storage: &self.storage_key,
                prompt: user_msg_id,
                dead_invocation: saved.invocation_id.as_str(),
                target: &target,
            })
            .await?;
        Ok(Some(bound))
    }

    /// Decide a fresh steering join: bind the live non-fixed head and return
    /// its invocation, or return `None` to fall through to reservation.
    /// Inherited steering never joins a fixed-admission head; it gets the
    /// ordinary busy refusal, exactly like a non-steering caller (issue #2672,
    /// review F2). External steering keeps today's behavior.
    pub(super) async fn steering_join_invocation(
        &self,
        user_msg_id: &str,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<String>> {
        let Some(active) = self
            .metadata_store
            .active_admission(&self.storage_key, entries)
            .await?
        else {
            return Ok(None);
        };
        let joinable = match self.admission_authority.as_ref() {
            Some(AdmissionAuthority::Inherited { .. }) => active.fixed_ticket.is_none(),
            _ => true,
        };
        if !joinable {
            return Ok(None);
        }
        self.metadata_store
            .bind_prompt_admission(
                &self.storage_key,
                user_msg_id,
                active.invocation_id.as_str(),
            )
            .await?;
        Ok(Some(active.invocation_id.as_str().to_owned()))
    }

    /// Fault injection belongs to one steering delivery attempt, never
    /// session state (CodeRabbit O1). Mirrors dispatch's `DispatchAttempt`.
    #[cfg(test)]
    pub(super) async fn inject_steering_fault(
        &self,
        user_msg_id: &str,
        fault: Option<SteeringFault>,
    ) -> Result<()> {
        if fault.is_none() {
            return Ok(());
        }
        // Deterministic turn-end race (O1/B): the steered turn ends between
        // the bind and the append, exactly like dispatch's `JoinedTurnEnds`.
        // A rowless invocation has no turn to end, so the fault only fires
        // when the bound invocation has a prompt row.
        let bound = self
            .metadata_store
            .prompt_admission(&self.storage_key, user_msg_id)
            .await?
            .context("steering fault requires a bound prompt")?;
        let entries = self.load_durable_entries().await?;
        let Some(seq) = self
            .metadata_store
            .invocation_prompt_seq(&self.storage_key, bound.invocation_id.as_str(), &entries)
            .await?
        else {
            return Ok(());
        };
        let log = crate::nats_session_log::NatsSessionLog::new_with_replicas(
            self.jetstream.clone(),
            &self.storage_key,
            self.attachment_replicas,
        );
        // The turn ends covering whatever it delivered: the original prompt
        // alone, or the original plus the competing retry's row.
        let mut through = seq;
        if fault == Some(SteeringFault::SteeredCompetingDeliveryEndsTurn) {
            // A competing retry of the same call delivers the committed row
            // while this caller pauses between its bind and append
            // (CodeRabbit B): the first append must refuse late steering.
            through = log
                .append_event_async(&SessionLogEntry::Message {
                    id: Some(user_msg_id.to_owned()),
                    role: harnx_core::message::MessageRole::User,
                    content: MessageContent::Text("competing steering".into()),
                    timestamp: None,
                    fence_token: None,
                })
                .await?;
        }
        log.append_event_async(&SessionLogEntry::TurnEnd {
            through_seq: through,
            fence_token: 1,
            timestamp: None,
            usage: None,
        })
        .await?;
        Ok(())
    }

    /// Strict steering join for coordinated frontends (A2A, issue #2774):
    /// bind the caller's admission id to the named invocation's live turn
    /// and append at the moving tail. No activation is published: the live
    /// turn's tool-round seam re-reads the log, so delivery needs no
    /// wake-up, and if the turn ends before consuming the row the row must
    /// not become queued worker work — the frontend owns the reply path, and
    /// a queued execution would run untracked and compete with the next
    /// admission. Unlike the opt-in join above, a dead or superseded head
    /// never falls through to a fresh reservation: [`SteeringTargetEnded`]
    /// leaves no row or activation behind, so the caller can re-decide
    /// admission itself. A committed row for this session's admission id
    /// replays as-is, making same-id retries idempotent. A superseded head
    /// surfaces the retryable [`SessionBusy`] instead.
    // qual:allow(iosp) reason: "Bind the steering identity, append the row and observe the join races in one ordered sequence; extracting pieces hides which refusal leaves which state behind."
    pub(super) async fn steer_admitted_content(
        &self,
        content: MessageContent,
        target_invocation: &str,
    ) -> Result<AppendedPrompt> {
        self.check_parent_work("child-prompt-admission").await?;
        let user_msg_id = self
            .invocation_id
            .clone()
            .unwrap_or_else(new_client_message_id);
        Self::ensure_unreserved_admission_id(&user_msg_id)?;
        if let Some(appended) = self
            .existing_prompt_matching_content(&user_msg_id, &content)
            .await?
        {
            return Ok(appended);
        }
        let entries = self.load_durable_entries().await?;
        anyhow::ensure!(
            !super::fixed_admission::admission_closed(&entries, target_invocation, &user_msg_id),
            "fixed admission closed without prompt; cannot replay through ordinary admission"
        );
        let active = self
            .metadata_store
            .active_admission(&self.storage_key, &entries)
            .await?;
        match active.as_ref().map(|active| active.invocation_id.as_str()) {
            Some(head) if head == target_invocation => {
                self.metadata_store
                    .bind_prompt_admission(&self.storage_key, &user_msg_id, target_invocation)
                    .await?;
            }
            Some(head) => {
                // Another live invocation owns the admission head, so the
                // caller cannot join this target; the context is busy with
                // different work and the frontend surfaces the retryable
                // busy error.
                return Err(
                    crate::nats_session_metadata::SessionBusy::ActiveInvocation {
                        invocation_id: head.to_owned(),
                    }
                    .into(),
                );
            }
            None => return Err(SteeringTargetEnded.into()),
        }
        let appended = match self
            .append_user_content_entry(content.clone(), user_msg_id.clone(), true)
            .await
        {
            Ok(appended) => appended,
            // The joined turn ended between the bind and the append (the same
            // race `recover_late_steering` handles for opt-in steering). A
            // competing same-id request may have committed this row before
            // the turn ended: replay that receipt so a delivered prompt is
            // never reported back to the caller as not delivered, which a
            // re-admission fallback could turn into a second delivery path.
            // Otherwise a coordinated caller must re-decide admission, not
            // retry the row.
            // qual:allow(coupling, deh) reason: "The anyhow append boundary carries the typed join-race refusal; matching it by downcast is the house pattern for typed races on anyhow errors."
            Err(error) if error.downcast_ref::<LateSteeringRefusal>().is_some() => {
                if let Some(committed) = self
                    .existing_prompt_matching_content(&user_msg_id, &content)
                    .await?
                {
                    return Ok(committed);
                }
                return Err(SteeringTargetEnded.into());
            }
            Err(error) => return Err(error),
        };
        Ok(appended)
    }
}

/// The invocation a steered prompt had to join is no longer the live,
/// non-terminal admission head. Typed so a coordinated frontend (A2A, issue
/// #2774) can fall back to its own admission path instead of receiving a
/// fresh reservation it never asked for.
///
/// When the head check itself observes no live head, nothing was bound,
/// appended or activated. In the bind-then-append race the append is
/// refused after the binding was committed: no row or activation exists,
/// but the admission binding remains, naming the now-ended invocation. The
/// binding resolves to nothing once that invocation is terminal, so fresh
/// admissions for the same message id proceed; a crash in the tiny window
/// before the fallback commits its own work can leave the stale binding to
/// surface a binding conflict on a same-id retry against a different live
/// head.
#[derive(Debug)]
pub struct SteeringTargetEnded;

// qual:allow(dry, boilerplate) reason: "One fixed sentence identifies the typed race error in logs; derive_more is not a workspace dependency."
impl std::fmt::Display for SteeringTargetEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "steering target invocation already ended")
    }
}
impl std::error::Error for SteeringTargetEnded {}

/// A committed row for this admission id exists with different content. The
/// coordinated frontend's changed-parts check lives in its dedupe mapping,
/// which is written after the row ("Dedupe ordering accepts one crash
/// window, by design"); while that mapping is absent, the replay below is
/// the only defence, so it refuses a changed retry instead of answering
/// with a row the caller never sent. Typed so the frontend surfaces its
/// fingerprint-mismatch error rather than a generic failure.
#[derive(Debug)]
pub struct SteeredContentMismatch;

// qual:allow(dry, boilerplate) reason: "One fixed sentence identifies the typed race error in logs; derive_more is not a workspace dependency."
impl std::fmt::Display for SteeredContentMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "committed steered row has different content")
    }
}
impl std::error::Error for SteeredContentMismatch {}

/// One steering delivery attempt. The fault field exists only in test builds;
/// production callers always pass `Default::default()`.
#[derive(Default)]
pub(super) struct SteeringAttempt {
    #[cfg(test)]
    pub(super) fault: Option<SteeringFault>,
}

#[cfg(test)]
impl From<SteeringFault> for SteeringAttempt {
    fn from(fault: SteeringFault) -> Self {
        Self { fault: Some(fault) }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SteeringFault {
    /// Test-only action, not a failure: end the joined invocation's turn
    /// between the bind and the steered append, reproducing the turn-end
    /// race the append backstop refuses (CodeRabbit O1).
    SteeredTurnEnds,
    /// Test-only action, not a failure: a competing retry of the same call
    /// delivers the prompt's committed row and the joined turn ends between
    /// the bind and the steered append (CodeRabbit B) — the late-steering
    /// recovery must replay the committed row, not re-deliver into the
    /// refusal.
    SteeredCompetingDeliveryEndsTurn,
}
