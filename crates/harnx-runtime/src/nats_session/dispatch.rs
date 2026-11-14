//! Durable composed-prompt delivery without attaching a follower.
use super::{AppendedPrompt, NatsSession};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{
    AdmissionAuthority, InvocationAdmission, JoinedDispatch, SessionBusy, VoidJoinRelease,
};
use anyhow::{ensure, Context, Result};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;

/// Bounded passes over the busy race before the dispatch fails for good.
const JOIN_BUSY_ROUNDS: usize = 4;

/// The typed busy refusal `reserve_admission` returns when another invocation
/// holds the admission head (issue #2672, review F3).
fn is_busy(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SessionBusy>().is_some()
}

/// No activation is needed for an already-visible boundary. This is not a
/// terminal classification: approval visibility also suppresses activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptActivation {
    Published,
    NotNeeded,
}

/// What the replay-or-join decision concluded before any reservation (issue
/// #2672, review round 2, C1). A plain `Ok(None)` used to conflate "no head
/// to join" with "a void join was redirected", sending the round into a
/// redundant reserve that could surface a spurious busy refusal against the
/// redirect's own live target.
enum ReplayOutcome {
    /// The prompt was appended or replayed; carries the receipt.
    Delivered(AppendedPrompt),
    /// Nothing owns the call yet; a fresh reservation must decide it.
    ProceedToReserve,
    /// State moved under the caller (a void-join redirect, or a busy race
    /// still inside the round budget); re-decide on fresh entries.
    Retry,
}

impl NatsSession {
    /// Reserve, bind and append once. Replay uses the original admission content
    /// and raw prompt sequence, never the invocation's latest steering prompt.
    pub(crate) async fn append_dispatch_content(
        &self,
        content: MessageContent,
    ) -> Result<AppendedPrompt> {
        self.append_dispatch_content_inner(content, DispatchAttempt::default())
            .await
    }

    /// Admission order for one dispatch attempt (plan #2672, D6):
    ///
    /// 1. A joined-dispatch record for this prompt id owns it: validate the
    ///    caller against the record, then replay the recorded invocation and
    ///    content. Never pick a different invocation on replay.
    /// 2. A saved admission for this prompt id replays a completed historical
    ///    invocation or the caller's own head; a leftover candidate intent
    ///    that never won the head falls through.
    /// 3. A turn this prompt id does not own is active: join it by writing the
    ///    create-only join record, binding the prompt admission and appending
    ///    under the recorded content.
    /// 4. Otherwise reserve a new invocation as before. A busy race between
    ///    the active check and the head CAS is retried through the join path.
    async fn append_dispatch_content_inner(
        &self,
        content: MessageContent,
        _attempt: DispatchAttempt,
    ) -> Result<AppendedPrompt> {
        let id = self
            .invocation_id
            .as_deref()
            .context("dispatch requires invocation identity")?;
        Self::ensure_unreserved_admission_id(id)?;
        let authority = self
            .admission_authority
            .as_ref()
            .context("dispatch requires inherited authority")?;
        ensure!(
            matches!(authority, AdmissionAuthority::Inherited { .. }),
            "dispatch requires inherited authority"
        );
        let parent = self
            .parent_session_id
            .as_deref()
            .context("dispatch requires trusted parent storage scope")?;
        let mut intent = InvocationAdmission::new(
            authority,
            id.into(),
            self.admission_timeout,
            self.admission_token_budget,
        );
        intent.parent_storage_key = Some(parent.into());
        intent.prompt_content = Some(content);
        for round in 0..JOIN_BUSY_ROUNDS {
            // Reload per round: a turn that ends mid-race must look terminal to
            // the next round's reserve head CAS, not just to the join check
            // (issue #2672, review F1).
            let entries = self.load_durable_entries().await?;
            if let Some(prompt) = self
                .append_dispatch_round(
                    &intent,
                    DispatchReplay {
                        entries: &entries,
                        attempt: &_attempt,
                    },
                    round,
                )
                .await?
            {
                return Ok(prompt);
            }
        }
        // Unreachable: the final round reserves instead of retrying, so it
        // always resolves to a prompt or an error (review round 2, C1/J-R1).
        unreachable!("the final dispatch round must resolve to a prompt or an error")
    }

    /// One bounded round of the busy race. The replay-or-join decision gets an
    /// explicit outcome; a reservation happens only when it asks for one, and
    /// a redirect retries the round instead of reserving against its own
    /// target (issue #2672, review round 2, C1). `Ok(None)` means "retry this
    /// round on fresh entries".
    async fn append_dispatch_round(
        &self,
        intent: &InvocationAdmission,
        replay: DispatchReplay<'_>,
        round: usize,
    ) -> Result<Option<AppendedPrompt>> {
        match self.replay_or_join_for_round(intent, replay, round).await? {
            ReplayOutcome::Delivered(prompt) => return Ok(Some(prompt)),
            ReplayOutcome::Retry => return Ok(None),
            ReplayOutcome::ProceedToReserve => {}
        }
        #[cfg(test)]
        if replay.attempt.fault == Some(DispatchFault::CompetingReservation) && round == 0 {
            // Deterministic busy race: another caller wins the admission head
            // between the active check and this prompt's own reservation.
            let mut competing = intent.clone();
            competing.invocation_id =
                crate::nats_session_metadata::InvocationIdentity::from_string("competing".into());
            self.metadata_store
                .reserve_admission(&self.storage_key, &competing, replay.entries)
                .await?;
        }
        // Reserve a fresh invocation only when idle or this prompt owns the head.
        let admission = match self
            .metadata_store
            .reserve_admission(&self.storage_key, intent, replay.entries)
            .await
        {
            Ok(admission) => admission,
            // A turn may have started between the active check and the head CAS.
            Err(error) if round + 1 < JOIN_BUSY_ROUNDS && is_busy(&error) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        self.finish_reserved_dispatch(&admission, intent, replay)
            .await
            .map(Some)
    }

    /// The replay-or-join outcome for this round, with the round budget
    /// folded in (issue #2672, review round 2, C1). A void-join redirect
    /// retries on fresh entries instead of reserving again — the reserve
    /// would busy against the redirect's own live target — and a busy error
    /// from the redirect's own reserve is classified into the same budget.
    /// On the final round a redirect falls through to the reserve, which
    /// converges (this prompt owns the head after a fresh-reservation
    /// redirect) or refuses busy.
    async fn replay_or_join_for_round(
        &self,
        intent: &InvocationAdmission,
        replay: DispatchReplay<'_>,
        round: usize,
    ) -> Result<ReplayOutcome> {
        match self
            .replay_or_join_dispatch(intent, replay.entries, replay.attempt)
            .await
        {
            Err(error) if round + 1 < JOIN_BUSY_ROUNDS && is_busy(&error) => {
                Ok(ReplayOutcome::Retry)
            }
            Err(error) => Err(error),
            Ok(ReplayOutcome::Retry) if round + 1 == JOIN_BUSY_ROUNDS => {
                Ok(ReplayOutcome::ProceedToReserve)
            }
            Ok(outcome) => Ok(outcome),
        }
    }

    async fn finish_reserved_dispatch(
        &self,
        admission: &InvocationAdmission,
        intent: &InvocationAdmission,
        replay: DispatchReplay<'_>,
    ) -> Result<AppendedPrompt> {
        let id = intent.invocation_id.as_str();
        #[cfg(test)]
        fail_at(replay.attempt.fault, DispatchFault::AfterAdmission)?;
        validate_dispatch_admission(admission, intent)?;
        self.metadata_store
            .bind_prompt_admission(&self.storage_key, id, admission.invocation_id.as_str())
            .await?;
        #[cfg(test)]
        fail_at(replay.attempt.fault, DispatchFault::AfterBinding)?;
        if let Some(prompt) = original_prompt(
            replay.entries,
            id,
            admission.invocation_id.as_str(),
            admission.prompt_content.as_ref(),
        )? {
            return Ok(prompt);
        }
        self.append_new_dispatch_prompt(id, admission, replay.attempt)
            .await
    }

    async fn replay_or_join_dispatch(
        &self,
        intent: &InvocationAdmission,
        entries: &[(u64, SessionLogEntry)],
        attempt: &DispatchAttempt,
    ) -> Result<ReplayOutcome> {
        let id = intent.invocation_id.as_str();
        // The joined record owns this dispatch even if a candidate intent
        // remains from a failed reservation.
        if let Some(joined) = self
            .metadata_store
            .joined_dispatch(&self.storage_key, id)
            .await?
        {
            validate_joined_dispatch(&joined, intent)?;
            return self.finish_joined_dispatch(intent, &joined, attempt).await;
        }
        let owns_head = self
            .metadata_store
            .admission_head(&self.storage_key)
            .await?
            .as_deref()
            == Some(id);
        if let Some(prompt) = self
            .replay_saved_dispatch(intent, entries, owns_head)
            .await?
        {
            return Ok(ReplayOutcome::Delivered(prompt));
        }
        if owns_head {
            return Ok(ReplayOutcome::ProceedToReserve);
        }
        // The join re-verifies on a fresh snapshot: a head that ended since
        // this round's snapshot, or a fixed head this prompt must not join,
        // falls through to the reserve below (issue #2672, review F1/F2).
        self.join_active_dispatch(intent, attempt).await
    }

    async fn replay_saved_dispatch(
        &self,
        intent: &InvocationAdmission,
        entries: &[(u64, SessionLogEntry)],
        owns_head: bool,
    ) -> Result<Option<AppendedPrompt>> {
        let id = intent.invocation_id.as_str();
        let Some(saved) = self.metadata_store.admission(&self.storage_key, id).await? else {
            return Ok(None);
        };
        validate_dispatch_admission(&saved, intent)?;
        // Replay completed history even while a later turn runs. An intent
        // that never won the head must fall through to the active-turn join.
        let replayable = owns_head
            || self
                .metadata_store
                .admission_is_terminal(&self.storage_key, id, entries)
                .await?;
        if !replayable {
            return Ok(None);
        }
        let prompt = original_prompt(
            entries,
            id,
            saved.invocation_id.as_str(),
            saved.prompt_content.as_ref(),
        )?;
        if prompt.is_some() {
            self.metadata_store
                .bind_prompt_admission(&self.storage_key, id, saved.invocation_id.as_str())
                .await?;
        }
        Ok(prompt)
    }

    /// Join the session's active turn. The decision rests on a fresh snapshot,
    /// never the round's possibly stale one, and a fixed-admission head is
    /// never joined: `ProceedToReserve` falls through to `reserve_admission`,
    /// which refuses it with the ordinary busy error exactly like a first
    /// send (issue #2672, review F1/F2).
    async fn join_active_dispatch(
        &self,
        intent: &InvocationAdmission,
        _attempt: &DispatchAttempt,
    ) -> Result<ReplayOutcome> {
        let entries = self.load_durable_entries().await?;
        let Some(active) = self
            .metadata_store
            .active_admission(&self.storage_key, &entries)
            .await?
        else {
            // The turn this round saw has ended; reserve a fresh invocation.
            return Ok(ReplayOutcome::ProceedToReserve);
        };
        if active.fixed_ticket.is_some() {
            return Ok(ReplayOutcome::ProceedToReserve);
        }
        let id = intent.invocation_id.as_str();
        if active.invocation_id.as_str() == id {
            // A prompt never joins its own invocation (CodeRabbit I1): a
            // caller whose cached `owns_head` read went stale can see its own
            // live reservation here, and a joined record minted from its raw
            // retry content would permanently fight the reservation's
            // frozen content in every replay and observation. Fall through
            // to the reserve, which replays the reservation and its frozen
            // content.
            return Ok(ReplayOutcome::ProceedToReserve);
        }
        let record = JoinedDispatch {
            parent_storage_key: intent.parent_storage_key.clone(),
            parent_invocation_id: intent
                .parent
                .as_ref()
                .map(|parent| parent.invocation_id.as_str().to_owned()),
            prompt_content: intent
                .prompt_content
                .clone()
                .context("dispatch requires original content")?,
            invocation_id: active.invocation_id.as_str().to_owned(),
        };
        // Create-only; a concurrent writer's record is reread and validated
        // like a replay of the joined path.
        let joined = self
            .metadata_store
            .create_joined_dispatch(&self.storage_key, id, &record)
            .await?;
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::AfterJoinedRecord)?;
        if joined != record {
            validate_joined_dispatch(&joined, intent)?;
        }
        self.finish_joined_dispatch(intent, &joined, _attempt).await
    }
    /// Complete a joined dispatch against its recorded invocation: replay an
    /// already-appended message, append the recorded content, or — when the
    /// recorded turn provably ended before delivery — release the prompt id to
    /// a fresh decision so the same call stays usable (issue #2672, review F1).
    /// The joined message runs under the joined invocation's existing
    /// deadline; no limits record is created or renewed.
    async fn finish_joined_dispatch(
        &self,
        intent: &InvocationAdmission,
        joined: &JoinedDispatch,
        _attempt: &DispatchAttempt,
    ) -> Result<ReplayOutcome> {
        self.check_parent_work("child-prompt-admission").await?;
        let id = intent.invocation_id.as_str();
        let entries = self.load_durable_entries().await?;
        // The captured record's invocation can be stale when a concurrent
        // retry of the same call redirected the binding (CodeRabbit I2); the
        // receipt reports the CURRENT binding so activation never rejects it.
        let execution = self
            .bound_execution_id(id, joined.invocation_id.as_str())
            .await?;
        if let Some(prompt) =
            original_prompt(&entries, id, &execution, Some(&joined.prompt_content))?
        {
            return Ok(ReplayOutcome::Delivered(prompt));
        }
        if self.join_is_void(id, joined, &entries).await? {
            // Proven-void join: the recorded turn ended before the recorded
            // message was appended, and the append backstop keeps that
            // delivery impossible forever. Release the prompt id so the round
            // re-decides on fresh entries — a fresh join when another turn is
            // live, else a fresh reservation — and the call id stays usable
            // (F1).
            self.redirect_void_join(intent, joined, &entries).await?;
            return Ok(ReplayOutcome::Retry);
        }
        self.bind_joined_prompt_admission(id, joined).await?;
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::AfterJoinedBinding)?;
        self.append_joined_recorded_prompt(intent, joined, _attempt)
            .await
    }

    /// The proven-void-join check (F1): the recorded turn is terminal and the
    /// recorded message is absent. Stable once true — the append backstop
    /// refuses appends behind a terminal row — so releasing the prompt id can
    /// neither strand nor duplicate an append.
    async fn join_is_void(
        &self,
        id: &str,
        joined: &JoinedDispatch,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<bool> {
        if user_message_present(entries, id) {
            return Ok(false);
        }
        self.metadata_store
            .admission_is_terminal(&self.storage_key, joined.invocation_id.as_str(), entries)
            .await
    }

    /// Execution identity a joined receipt must report: the prompt's CURRENT
    /// admission binding, not the captured record's invocation. A concurrent
    /// retry of the same call may have redirected both, and activation
    /// re-checks the binding, so a stale name is rejected as
    /// `dispatch=unconfirmed` (CodeRabbit I2). A missing binding falls back
    /// to the recorded invocation; joined deliveries always bind first, so
    /// this only preserves a diagnostic.
    async fn bound_execution_id(&self, id: &str, recorded: &str) -> Result<String> {
        Ok(self
            .metadata_store
            .prompt_admission_binding(&self.storage_key, id)
            .await?
            .unwrap_or_else(|| recorded.to_owned()))
    }

    /// Append the recorded joined content, or — when the recorded turn ends
    /// first — release the prompt id so the round re-decides, never surfacing
    /// the misleading "late steering" refusal (F1).
    async fn append_joined_recorded_prompt(
        &self,
        intent: &InvocationAdmission,
        joined: &JoinedDispatch,
        _attempt: &DispatchAttempt,
    ) -> Result<ReplayOutcome> {
        #[cfg(test)]
        if _attempt.fault == Some(DispatchFault::JoinedTurnEnds) {
            // Deterministic turn-end race: the joined turn ends between the
            // joinability check and the append. A rowless invocation has no
            // turn to end, so the fault only fires when a prompt row exists.
            let entries = self.load_durable_entries().await?;
            if let Some(seq) = self
                .metadata_store
                .invocation_prompt_seq(&self.storage_key, joined.invocation_id.as_str(), &entries)
                .await?
            {
                NatsSessionLog::new_with_replicas(
                    self.jetstream.clone(),
                    &self.storage_key,
                    self.attachment_replicas,
                )
                .append_event_async(&SessionLogEntry::TurnEnd {
                    through_seq: seq,
                    fence_token: 1,
                    timestamp: None,
                    usage: None,
                })
                .await?;
            }
        }
        let id = intent.invocation_id.as_str();
        match self
            .append_recorded_dispatch_prompt(
                RecordedDispatchPrompt {
                    id,
                    invocation_id: &joined.invocation_id,
                    content: Some(&joined.prompt_content),
                },
                _attempt,
            )
            .await
        {
            Ok(prompt) => {
                // The receipt must carry the CURRENT binding, not the captured
                // record's invocation, which a concurrent retry may have
                // redirected (CodeRabbit I2).
                let execution = self
                    .bound_execution_id(id, joined.invocation_id.as_str())
                    .await?;
                Ok(ReplayOutcome::Delivered(AppendedPrompt {
                    execution_id: Some(execution),
                    ..prompt
                }))
            }
            Err(error) => {
                let entries = self.load_durable_entries().await?;
                if self.join_is_void(id, joined, &entries).await? {
                    self.redirect_void_join(intent, joined, &entries).await?;
                    return Ok(ReplayOutcome::Retry);
                }
                Err(error)
            }
        }
    }

    /// Bind the prompt to exactly the invocation the record names. A binding
    /// still naming a provably dead, undelivered invocation — the crash window
    /// between a void-join record retarget and its binding move — is
    /// redirected first; any other mismatch stays a fail-closed conflict.
    async fn bind_joined_prompt_admission(&self, id: &str, joined: &JoinedDispatch) -> Result<()> {
        let target = joined.invocation_id.as_str();
        if self
            .metadata_store
            .bind_prompt_admission(&self.storage_key, id, target)
            .await
            .is_ok()
        {
            return Ok(());
        }
        let Some(dead) = self
            .metadata_store
            .prompt_admission_binding(&self.storage_key, id)
            .await?
        else {
            anyhow::bail!("prompt admission binding conflict");
        };
        let entries = self.load_durable_entries().await?;
        ensure!(
            self.metadata_store
                .admission_is_terminal(&self.storage_key, &dead, &entries)
                .await?
                && !user_message_present(&entries, id),
            "prompt admission binding conflict"
        );
        let release = VoidJoinRelease {
            storage: &self.storage_key,
            prompt: id,
            dead_invocation: &dead,
            target: joined.invocation_id.as_str(),
        };
        let bound = self
            .metadata_store
            .redirect_prompt_admission(release)
            .await?;
        ensure!(bound == target, "prompt admission binding conflict");
        Ok(())
    }

    /// Release a proven-void join (issue #2672, review F1). The caller has
    /// verified on fresh entries that the recorded turn is terminal and the
    /// recorded message absent. One retry of the same call owns the release:
    /// the join record is CAS-rewritten to name the new target first, and only
    /// the winner moves the prompt binding off the dead invocation.
    async fn redirect_void_join(
        &self,
        intent: &InvocationAdmission,
        joined: &JoinedDispatch,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<()> {
        let id = intent.invocation_id.as_str();
        let target = self.void_join_target(intent, joined, entries).await?;
        let release = VoidJoinRelease {
            storage: &self.storage_key,
            prompt: id,
            dead_invocation: joined.invocation_id.as_str(),
            target: &target,
        };
        if !self
            .metadata_store
            .retarget_joined_dispatch(release, joined)
            .await?
        {
            // A concurrent retry won the record CAS, or the record was purged
            // (a delete marker reads as not-the-winner too); either way no
            // delivery is in flight. This round re-reads fresh state and
            // re-decides.
            return Ok(());
        }
        self.move_void_join_binding(id, &target).await
    }

    /// Fresh delivery target for a void-join release, mirroring a brand-new
    /// dispatch: join the live non-fixed turn, else reserve this prompt's own
    /// invocation. A fixed head refuses with the ordinary busy error, like a
    /// first send to it.
    async fn void_join_target(
        &self,
        intent: &InvocationAdmission,
        joined: &JoinedDispatch,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<String> {
        if let Some(active) = self
            .metadata_store
            .active_admission(&self.storage_key, entries)
            .await?
        {
            if active.fixed_ticket.is_none() {
                return Ok(active.invocation_id.as_str().to_owned());
            }
        }
        // The record's content is authoritative (D5): the fresh reservation
        // must store it, whatever text a raw retry passed. A leftover
        // candidate from an earlier busy reserve predates the join and is
        // superseded first (CodeRabbit I3): the void proof already showed no
        // row was ever appended under this prompt, so its stale content is
        // safe to replace, and a fixed ticket refuses inside the store,
        // keeping the immutable content contract.
        let id = intent.invocation_id.as_str();
        self.metadata_store
            .recontent_candidate_admission(&self.storage_key, id, &joined.prompt_content)
            .await?;
        let mut reserved_intent = intent.clone();
        reserved_intent.prompt_content = Some(joined.prompt_content.clone());
        let reserved = self
            .metadata_store
            .reserve_admission(&self.storage_key, &reserved_intent, entries)
            .await?;
        validate_dispatch_admission(&reserved, &reserved_intent)?;
        // A leftover candidate intent from an earlier busy reserve of the
        // same call must agree with the record that owns it.
        ensure!(
            reserved.prompt_content.as_ref() == Some(&joined.prompt_content),
            "leftover dispatch admission content disagrees with the joined record"
        );
        Ok(reserved.invocation_id.as_str().to_owned())
    }

    /// Move the prompt binding onto the redirect's target. It can lag the
    /// record across the crash window between the two CAS writes (review
    /// round 2, C2) and still name an older dead generation, so re-prove
    /// whatever it names and fence the move on that value — the same
    /// half-state repair `bind_joined_prompt_admission` applies at delivery
    /// time — instead of failing with a binding conflict.
    async fn move_void_join_binding(&self, id: &str, target: &str) -> Result<()> {
        for _ in 0..JOIN_BUSY_ROUNDS {
            let Some(current) = self
                .metadata_store
                .prompt_admission_binding(&self.storage_key, id)
                .await?
            else {
                // Never bound: the delivery path's create-only bind writes
                // the target.
                return Ok(());
            };
            if current == target {
                return Ok(());
            }
            let entries = self.load_durable_entries().await?;
            ensure!(
                self.metadata_store
                    .admission_is_terminal(&self.storage_key, &current, &entries)
                    .await?
                    && !user_message_present(&entries, id),
                "prompt admission binding conflict"
            );
            let bound = self
                .metadata_store
                .redirect_prompt_admission(VoidJoinRelease {
                    storage: &self.storage_key,
                    prompt: id,
                    dead_invocation: &current,
                    target,
                })
                .await?;
            if bound == target {
                return Ok(());
            }
            // Moved elsewhere mid-flight (a concurrent retry of the same
            // call): re-read and re-prove before trying again.
        }
        anyhow::bail!("prompt admission binding kept changing")
    }

    async fn append_new_dispatch_prompt(
        &self,
        id: &str,
        admission: &InvocationAdmission,
        _attempt: &DispatchAttempt,
    ) -> Result<AppendedPrompt> {
        self.check_parent_work("child-prompt-admission").await?;
        ensure!(
            !dispatch_deadline_expired(&self.metadata_store, &self.storage_key, admission).await?,
            "dispatch admission deadline expired before append"
        );
        self.append_recorded_dispatch_prompt(
            RecordedDispatchPrompt {
                id,
                invocation_id: admission.invocation_id.as_str(),
                content: admission.prompt_content.as_ref(),
            },
            _attempt,
        )
        .await
    }

    async fn append_recorded_dispatch_prompt(
        &self,
        prompt: RecordedDispatchPrompt<'_>,
        _attempt: &DispatchAttempt,
    ) -> Result<AppendedPrompt> {
        let RecordedDispatchPrompt {
            id,
            invocation_id,
            content,
        } = prompt;
        let log = NatsSessionLog::new_with_replicas(
            self.jetstream.clone(),
            &self.storage_key,
            self.attachment_replicas,
        );
        let entry = SessionLogEntry::Message {
            id: Some(id.into()),
            role: MessageRole::User,
            content: content
                .context("dispatch admission has no original content")?
                .clone(),
            timestamp: None,
            fence_token: None,
        };
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::BeforeAppend)?;
        let appended = self.append_prompt_entry(&log, &entry, id).await;
        // A lost PubAck is not proof that append failed. Resolve the exact raw
        // identity before returning an error; never pick a later prompt.
        #[cfg(test)]
        let appended = if _attempt.fault == Some(DispatchFault::AppendAckLost) {
            appended.and_then(|_| anyhow::bail!("injected dispatch append acknowledgement loss"))
        } else {
            appended
        };
        match appended {
            Ok(seq) => Ok(dispatch_prompt(id, seq, invocation_id)),
            Err(error) => {
                let entries = self.load_durable_entries().await?;
                if let Some(prompt) = original_prompt(&entries, id, invocation_id, content)? {
                    Ok(prompt)
                } else {
                    Err(error.context("dispatch prompt not durably appended"))
                }
            }
        }
    }

    /// Publish only this original prompt. A completed/blocked replay doesn't
    /// resume approvals, and an expired admission doesn't start fresh work.
    pub(crate) async fn activate_dispatch_prompt(
        &self,
        prompt: &AppendedPrompt,
    ) -> Result<PromptActivation> {
        self.activate_dispatch_prompt_inner(prompt, DispatchAttempt::default())
            .await
    }

    async fn activate_dispatch_prompt_inner(
        &self,
        prompt: &AppendedPrompt,
        _attempt: DispatchAttempt,
    ) -> Result<PromptActivation> {
        let admission = self
            .metadata_store
            .prompt_admission(&self.storage_key, prompt.user_msg_id())
            .await?
            .context("dispatch prompt has no admission binding")?;
        ensure!(
            Some(admission.invocation_id.as_str()) == prompt.execution_id(),
            "dispatch execution binding mismatch"
        );
        let joined = self
            .metadata_store
            .joined_dispatch(&self.storage_key, prompt.user_msg_id())
            .await?;
        self.validate_dispatch_parent_scope(&admission, joined.as_ref())?;
        ensure!(
            admission.fixed_ticket.is_none(),
            "fixed admission requires fixed-ticket activation"
        );
        let entries = self.load_durable_entries().await?;
        let original = match &joined {
            Some(joined) => original_prompt(
                &entries,
                prompt.user_msg_id(),
                joined.invocation_id.as_str(),
                Some(&joined.prompt_content),
            )?,
            None => original_prompt(
                &entries,
                prompt.user_msg_id(),
                admission.invocation_id.as_str(),
                admission.prompt_content.as_ref(),
            )?,
        }
        .context("dispatch original prompt is missing")?;
        ensure!(
            original.user_msg_seq() == prompt.user_msg_seq(),
            "dispatch original prompt sequence mismatch"
        );
        // Visibility is used only as an activation guard, never as terminal proof.
        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
        if Self::is_turn_completion_visible(&entries, Some(&effective), prompt.user_msg_seq()) {
            return Ok(PromptActivation::NotNeeded);
        }
        ensure!(
            !dispatch_deadline_expired(&self.metadata_store, &self.storage_key, &admission).await?,
            "dispatch admission deadline expired before activation"
        );
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::BeforeActivation)?;
        self.publish_dispatch_activation(prompt).await?;
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::ActivationAckLost)?;
        Ok(PromptActivation::Published)
    }

    fn validate_dispatch_parent_scope(
        &self,
        admission: &InvocationAdmission,
        joined: Option<&JoinedDispatch>,
    ) -> Result<()> {
        // A joined-dispatch record supplies the caller's dispatch identity;
        // the invocation-level parent check is the fallback only without one.
        let caller_parent_invocation = match self.admission_authority.as_ref() {
            Some(AdmissionAuthority::Inherited { parent, .. }) => {
                Some(parent.invocation_id.as_str())
            }
            Some(AdmissionAuthority::External { .. }) | None => None,
        };
        match joined {
            Some(joined) => ensure!(
                joined.matches_caller(self.parent_session_id.as_deref(), caller_parent_invocation),
                "dispatch parent storage scope mismatch"
            ),
            None => ensure!(
                admission.parent_storage_key == self.parent_session_id
                    && self.parent_session_id.is_some(),
                "dispatch parent storage scope mismatch"
            ),
        }
        Ok(())
    }

    async fn publish_dispatch_activation(&self, prompt: &AppendedPrompt) -> Result<()> {
        use crate::nats_worker::{
            publish_session_activate, publish_targeted_session_activate, LocalWorkerTarget,
            SessionActivate, SessionActivationRoute,
        };
        self.check_parent_work("child-activation").await?;
        // Journal replay republishes the exact activation, including its dedup
        // identity. Explicit recovery attempts may use a fresh delivery epoch,
        // but must still target this original prompt, never a session tail.
        let mut activation = SessionActivate {
            session_id: self.storage_key.clone(),
            execution_id: prompt.execution_id().map(str::to_owned),
            epoch: format!(
                "dispatch:{}:{}",
                prompt.user_msg_id(),
                prompt.user_msg_seq()
            ),
            agent_name: self.config.initializer.agent_name().map(str::to_owned),
            requested_seq: Some(prompt.user_msg_seq()),
            target_worker_id: None,
            token_budget: None,
            tool_confirmation_subject: None,
        };
        match &self.config.activation_route {
            SessionActivationRoute::ClusterShared => {
                publish_session_activate(
                    &self.jetstream,
                    &self.config.cluster,
                    &activation,
                    self.attachment_replicas,
                )
                .await?;
            }
            SessionActivationRoute::WorkerTargeted {
                session_scope,
                worker_id,
            } => {
                activation.target_worker_id = Some(worker_id.clone());
                publish_targeted_session_activate(
                    &self.jetstream,
                    LocalWorkerTarget::new(session_scope, worker_id)?,
                    &activation,
                )
                .await?;
            }
        }
        Ok(())
    }
}

fn validate_dispatch_admission(
    saved: &InvocationAdmission,
    intent: &InvocationAdmission,
) -> Result<()> {
    ensure!(
        saved.fixed_ticket.is_none(),
        "fixed admission requires fixed-ticket append"
    );
    ensure!(
        saved.origin == intent.origin
            && saved.parent_storage_key == intent.parent_storage_key
            && saved.run_id == intent.run_id
            && saved.parent.as_ref().map(|p| &p.invocation_id)
                == intent.parent.as_ref().map(|p| &p.invocation_id),
        "dispatch admission parent scope mismatch"
    );
    Ok(())
}

/// A joined-dispatch record is the caller's immutable dispatch identity. The
/// same parent-scope mismatch message and fields as the invocation-level
/// check keep the failure modes indistinguishable to callers.
fn validate_joined_dispatch(joined: &JoinedDispatch, intent: &InvocationAdmission) -> Result<()> {
    ensure!(
        joined.matches_caller(
            intent.parent_storage_key.as_deref(),
            intent.parent.as_ref().map(|p| p.invocation_id.as_str()),
        ),
        "dispatch admission parent scope mismatch"
    );
    Ok(())
}

fn dispatch_prompt(id: &str, seq: u64, invocation_id: &str) -> AppendedPrompt {
    AppendedPrompt {
        user_msg_id: id.into(),
        user_msg_seq: seq,
        execution_id: Some(invocation_id.into()),
        events: None,
        live: None,
    }
}

/// Whether a user row with this prompt id is already committed. Shared by the
/// dispatch and steering join paths' proven-void proof (issue #2672, review
/// round 2, C13): with the invocation terminal, this absence is stable,
/// because the append backstop refuses appends behind a terminal row.
pub(super) fn user_message_present(entries: &[(u64, SessionLogEntry)], id: &str) -> bool {
    entries.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::Message { id: Some(found), role, .. } if found == id && role.is_user()
        )
    })
}

fn original_prompt(
    entries: &[(u64, SessionLogEntry)],
    id: &str,
    invocation_id: &str,
    expected: Option<&MessageContent>,
) -> Result<Option<AppendedPrompt>> {
    for (seq, entry) in entries {
        if let SessionLogEntry::Message {
            id: Some(found),
            role,
            content,
            ..
        } = entry
        {
            if role.is_user() && found == id {
                ensure!(
                    Some(content) == expected,
                    "dispatch original prompt content mismatch"
                );
                return Ok(Some(dispatch_prompt(id, *seq, invocation_id)));
            }
        }
    }
    Ok(None)
}

pub(crate) async fn dispatch_deadline_expired(
    store: &crate::nats_session_metadata::SessionMetadataStore,
    storage: &str,
    admission: &InvocationAdmission,
) -> Result<bool> {
    let limits = store
        .get_invocation_limits(storage, admission.invocation_id.as_str())
        .await?;
    // The lookup can cross the hard deadline. Check time after broker I/O,
    // including during fresh observer repair, not at request start.
    let now = chrono::Utc::now();
    if let Some(limits) = limits {
        return Ok(limits.is_expired_at(now));
    }
    // Target policy is resolved only by the worker. Before that, enforce the
    // immutable ancestor clamp and any explicit local allowance, not a guessed
    // fallback that could shorten a configured target policy.
    if admission
        .parent
        .as_ref()
        .is_some_and(|parent| parent.is_expired_at(now))
    {
        return Ok(true);
    }
    if let Some(seconds) = admission.timeout_secs.filter(|seconds| *seconds > 0) {
        let duration = chrono::Duration::try_seconds(i64::try_from(seconds)?)
            .context("dispatch deadline overflow")?;
        let deadline = admission
            .admitted_at
            .checked_add_signed(duration)
            .context("dispatch deadline overflow")?;
        return Ok(now >= deadline);
    }
    Ok(false)
}

/// The immutable identity and content used by either admission path.
struct RecordedDispatchPrompt<'a> {
    id: &'a str,
    invocation_id: &'a str,
    content: Option<&'a MessageContent>,
}

#[derive(Clone, Copy)]
struct DispatchReplay<'a> {
    entries: &'a [(u64, SessionLogEntry)],
    attempt: &'a DispatchAttempt,
}

// Fault injection belongs to one delivery attempt, never session state.
#[derive(Default)]
struct DispatchAttempt {
    #[cfg(test)]
    fault: Option<DispatchFault>,
}

#[cfg(test)]
impl From<DispatchFault> for DispatchAttempt {
    fn from(fault: DispatchFault) -> Self {
        Self { fault: Some(fault) }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchFault {
    AfterAdmission,
    AfterBinding,
    BeforeAppend,
    AppendAckLost,
    BeforeActivation,
    ActivationAckLost,
    AfterJoinedRecord,
    AfterJoinedBinding,
    /// Test-only action, not a failure: end the joined invocation's turn just
    /// before the joined append, reproducing the turn-end race (F1).
    JoinedTurnEnds,
    /// Test-only action, not a failure: a competing caller wins the admission
    /// head between this round's active check and its reservation (F9).
    CompetingReservation,
}

#[cfg(test)]
fn fail_at(actual: Option<DispatchFault>, boundary: DispatchFault) -> Result<()> {
    ensure!(
        actual != Some(boundary),
        "injected dispatch failure at {boundary:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
