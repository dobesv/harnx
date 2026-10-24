//! Accepted replies are control intent on an existing ticket, never new prompts.
use super::*;
use crate::{
    hitl::{parse_reply, ReplyParse},
    store::context::{AuthorityError, HitlPhase, ReplyReceipt},
};
use harnx_runtime::{ApprovalRef, HitlDecisionOutcome, PromptProgress};

impl Runner {
    /// `request` is captured before attempting ownership and never reselected.
    pub(crate) async fn reply_hitl(
        self: &Arc<Self>,
        turn: TurnRequest<'_>,
        request: ApprovalRef,
    ) -> Result<StartTurnResult> {
        let TurnRequest {
            export,
            owner,
            session,
            message,
        } = turn;
        self.authorize_session(export, owner, &session).await?;
        let fingerprint = crate::store::message_fingerprint(&message.parts);
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::HitlReplyBound)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let lease = loop {
            if let Some(snapshot) = self
                .reply_retry_snapshot(&session, &message, &fingerprint)
                .await?
            {
                return Ok(reply_snapshot(snapshot));
            }
            if let Some(lease) = self.candidate_lease(session.storage_key()).await? {
                break lease;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(RunnerError::Busy.into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let mut authority = self.claim_context(&session, lease).await?;
        let result = self
            .accept_reply(&session, &mut authority, &request, &message, &fingerprint)
            .await;
        let resume = match result {
            Ok(resume) => resume,
            Err(error) => {
                authority
                    .release(&self.store, session.storage_key())
                    .await?;
                // Another replica can finish this message while we wait for its
                // lease, including moving the task to the next challenge.
                if let Some(snapshot) = self
                    .reply_retry_snapshot(&session, &message, &fingerprint)
                    .await?
                {
                    return Ok(reply_snapshot(snapshot));
                }
                return Err(error);
            }
        };
        if resume {
            self.resume_owned_hitl(export, session, authority).await
        } else {
            let snapshot = authority
                .context
                .document
                .state
                .active
                .as_ref()
                .context("reply task missing")?
                .snapshot
                .clone();
            authority
                .release(&self.store, session.storage_key())
                .await?;
            Ok(reply_snapshot(snapshot))
        }
    }

    async fn reply_retry_snapshot(
        &self,
        session: &NatsSession,
        message: &Message,
        fingerprint: &str,
    ) -> Result<Option<TaskRecord>> {
        let Some((task_id, saved)) = self
            .store
            .get_reply_dedupe(session.storage_key(), &message.message_id)
            .await?
        else {
            return Ok(None);
        };
        anyhow::ensure!(saved == fingerprint, StoreError::FingerprintMismatch);
        let unresolved = self
            .store
            .read_context(session.storage_key())
            .await?
            .and_then(|c| c.document.state.active)
            .is_some_and(|active| {
                active.snapshot.task.id == task_id
                    && !active.snapshot.task.status.state.is_terminal()
                    && active.hitl_checkpoint.as_ref().is_some_and(|c| {
                        c.phase == HitlPhase::DecisionIntent
                            && c.reply
                                .as_ref()
                                .is_some_and(|r| r.message_id == message.message_id)
                    })
            });
        if unresolved {
            return Ok(None);
        }
        Ok(Some(
            self.store
                .get_task(session.storage_key(), &task_id)
                .await?
                .context("reply dedupe task missing")?,
        ))
    }

    async fn accept_reply(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        request: &ApprovalRef,
        message: &Message,
        fingerprint: &str,
    ) -> Result<bool> {
        authority
            .flush_pending(&self.store, session.storage_key())
            .await?;
        self.persist_reply_intent(session, authority, request, message, fingerprint)
            .await?;
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::HitlReplyIntentCommitted)
            .await;
        let active = authority
            .context
            .document
            .state
            .active
            .as_ref()
            .context("reply task missing")?;
        let checkpoint = active
            .hitl_checkpoint
            .as_ref()
            .context("reply checkpoint missing")?;
        let receipt = checkpoint
            .reply
            .as_ref()
            .context("reply receipt missing")?
            .clone();
        self.store
            .put_reply_dedupe(
                session.storage_key(),
                &receipt.message_id,
                &active.snapshot.task.id,
                &receipt.fingerprint,
            )
            .await?;
        if checkpoint.phase == HitlPhase::Following
            || active.snapshot.task.status.state.is_terminal()
        {
            return Ok(false);
        }
        // The inline receipt is bounded. Deliver the full plain-text note retained
        // atomically with intent, including on retries after the mapping write.
        let saved = active
            .snapshot
            .task
            .history
            .as_ref()
            .and_then(|history| history.iter().find(|m| m.message_id == receipt.message_id))
            .context("accepted reply history missing")?;
        let note = match parse_reply(&saved.parts) {
            ReplyParse::Approve(note) | ReplyParse::Reject(note) => note,
            ReplyParse::Unrecognized => anyhow::bail!("accepted reply is not plain text"),
        };
        let outcome = session
            .decide_hitl_approval_for(request, receipt.approved, note)
            .await?;
        let progress = session
            .fixed_prompt_progress(&authority.ticket(session.storage_key())?)
            .await?;
        if matches!(&progress, PromptProgress::InputRequired(p) if p.approval_ref(&request.invocation_id) == *request)
            && matches!(
                outcome,
                HitlDecisionOutcome::Conflicting | HitlDecisionOutcome::Stale
            )
        {
            // No proof that this request advanced. Leave exact intent for a retry;
            // never answer a replacement request with this message.
            return Ok(false);
        }
        match outcome {
            HitlDecisionOutcome::Applied | HitlDecisionOutcome::AlreadyApplied { .. } => {
                self.advance_reply(session, authority, request, &outcome)
                    .await?;
            }
            HitlDecisionOutcome::Conflicting | HitlDecisionOutcome::Stale => {
                // Another frontend may already have advanced to Q2 or terminal.
                // Attach the original follower to project that progress, not a
                // synthesized terminal state or another decision delivery.
                authority
                    .update(&self.store, session.storage_key(), |active| {
                        let checkpoint =
                            active.hitl_checkpoint.as_mut().expect("checked checkpoint");
                        assert_eq!(checkpoint.request, *request);
                        checkpoint.phase = HitlPhase::Following;
                        checkpoint.reply.as_mut().expect("checked reply").outcome = Some(
                            match outcome {
                                HitlDecisionOutcome::Conflicting => "conflicting",
                                _ => "stale",
                            }
                            .into(),
                        );
                        active.snapshot.task.status = status(TaskState::Working, None);
                        let id = format!(
                            "{}:hitl:{}:working",
                            active.snapshot.task.id, request.request_seq
                        );
                        super::outbox::stage_event(
                            active,
                            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                                task_id: active.snapshot.task.id.clone(),
                                context_id: active.snapshot.task.context_id.clone(),
                                status: active.snapshot.task.status.clone(),
                                metadata: None,
                            }),
                            &id,
                        );
                    })
                    .await?;
            }
        }
        authority
            .flush_pending(&self.store, session.storage_key())
            .await?;
        Ok(true)
    }

    async fn persist_reply_intent(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        request: &ApprovalRef,
        message: &Message,
        fingerprint: &str,
    ) -> Result<()> {
        let (approved, note) = match parse_reply(&message.parts) {
            ReplyParse::Approve(note) => (true, note),
            ReplyParse::Reject(note) => (false, note),
            ReplyParse::Unrecognized => anyhow::bail!("reply not recognized"),
        };
        let receipt = ReplyReceipt::new(
            message.message_id.clone(),
            fingerprint.into(),
            request.clone(),
            approved,
            note,
        );
        let expected = authority.context.version()?;
        for _ in 0..8 {
            anyhow::ensure!(
                authority.lease.revalidate_ownership().await?,
                "A2A scoped lease lost"
            );
            let current = self
                .store
                .read_context(session.storage_key())
                .await?
                .context("reply context missing")?;
            let version = current.version()?;
            anyhow::ensure!(
                version.owner == expected.owner && version.task_id == expected.task_id,
                AuthorityError::StaleOwner
            );
            let active = current
                .document
                .state
                .active
                .as_ref()
                .context("reply task missing")?;
            let checkpoint = active
                .hitl_checkpoint
                .as_ref()
                .context("reply checkpoint missing")?;
            anyhow::ensure!(checkpoint.request == *request, AuthorityError::Conflict);
            if let Some(saved) = &checkpoint.reply {
                anyhow::ensure!(
                    saved.message_id == message.message_id,
                    AuthorityError::Conflict
                );
                anyhow::ensure!(
                    saved.fingerprint == fingerprint,
                    StoreError::FingerprintMismatch
                );
                authority.context = current;
                return Ok(());
            }
            let prepared = self
                .store
                .prepare_context_update_checked(
                    session.storage_key(),
                    &version,
                    &uuid::Uuid::new_v4().to_string(),
                    |state| {
                        let active = state.active.as_mut().context("reply task missing")?;
                        anyhow::ensure!(active.cancel.is_none(), RunnerError::Busy);
                        anyhow::ensure!(
                            active.snapshot.task.status.state == TaskState::InputRequired,
                            AuthorityError::Conflict
                        );
                        let checkpoint = active
                            .hitl_checkpoint
                            .as_mut()
                            .context("reply checkpoint missing")?;
                        anyhow::ensure!(
                            checkpoint.request == *request
                                && checkpoint.phase == HitlPhase::Waiting
                                && checkpoint.reply.is_none(),
                            AuthorityError::Conflict
                        );
                        checkpoint.phase = HitlPhase::DecisionIntent;
                        checkpoint.reply = Some(receipt.clone());
                        active
                            .snapshot
                            .task
                            .history
                            .get_or_insert_with(Vec::new)
                            .push(message.clone());
                        Ok(())
                    },
                )
                .await;
            let committed = match prepared {
                Ok(write) => self.store.commit_context(&write).await,
                Err(error) => Err(error),
            };
            match committed {
                Ok(context) => {
                    authority.context = context;
                    return Ok(());
                }
                Err(error)
                    if error.downcast_ref::<AuthorityError>()
                        == Some(&AuthorityError::Conflict) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!(AuthorityError::Conflict)
    }

    async fn advance_reply(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        request: &ApprovalRef,
        outcome: &HitlDecisionOutcome,
    ) -> Result<()> {
        for _ in 0..8 {
            authority
                .flush_pending(&self.store, session.storage_key())
                .await?;
            match self
                .store
                .advance_hitl_to_following(
                    session.storage_key(),
                    &authority.context.version()?,
                    request,
                    outcome,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error)
                    if error.downcast_ref::<AuthorityError>()
                        == Some(&AuthorityError::Conflict) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!(AuthorityError::Conflict)
    }
}

fn reply_snapshot(snapshot: TaskRecord) -> StartTurnResult {
    let (_, events) = broadcast::channel(EVENT_CAPACITY);
    StartTurnResult {
        snapshot,
        events,
        deduped: true,
    }
}
