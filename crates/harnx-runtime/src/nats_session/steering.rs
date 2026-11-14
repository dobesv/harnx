//! Steering joins a prompt to a session's active turn instead of reserving a
//! new invocation (issue #2672). This module holds the decisions
//! `admit_prompt` delegates to: the replay of an already-admitted prompt
//! (with recovery of a join whose turn provably ended undelivered, review
//! F1/M-A) and the fresh steering-join decision (which never joins a
//! fixed-admission head, review F2).
use super::dispatch::user_message_present;
use super::NatsSession;
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
}

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
