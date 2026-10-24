//! Resume only durable HITL continuations under their original fixed ticket.
use super::*;
use crate::{
    hitl::{parse_reply, ReplyParse},
    store::context::{ActiveTask, HitlCheckpoint, HitlPhase, ReplyReceipt},
};
use harnx_runtime::{
    nats_session::fixed_admission::FixedAdmissionOutcome, HitlDecisionReceipt, PendingHitlApproval,
    PromptProgress,
};

enum RecoveryAction {
    Settle,
    Parked,
    Follow,
}

impl Runner {
    /// Reinspect released approvals on every sweep without churning owner CASes.
    pub(in crate::runner) async fn unchanged_parked(&self, session: &NatsSession) -> Result<bool> {
        let Some(context) = self.store.read_context(session.storage_key()).await? else {
            return Ok(false);
        };
        let Some(active) = context.document.state.active.as_ref() else {
            return Ok(false);
        };
        let Some(checkpoint) = active.hitl_checkpoint.as_ref() else {
            return Ok(false);
        };
        if context.document.owner.is_some()
            || active.cancel.is_some()
            || active.publication.pending.is_some()
            || active.snapshot.task.status.state != TaskState::InputRequired
            || checkpoint.phase != HitlPhase::Waiting
            || checkpoint.reply.is_some()
        {
            return Ok(false);
        }
        let authority = OwnedTask::ticket_from_context(&context, session.storage_key())?;
        Ok(matches!(session.fixed_prompt_progress(&authority).await?,
            PromptProgress::InputRequired(p) if p.approval_ref(authority.invocation_id()) == checkpoint.request))
    }

    pub(in crate::runner) async fn recover_registered_task(
        self: &Arc<Self>,
        export: &Export,
        session: NatsSession,
        mut authority: OwnedTask,
    ) -> Result<()> {
        match Box::pin(self.recover_hitl_progress(&session, &mut authority)).await {
            Ok(RecoveryAction::Follow) => {
                Box::pin(self.resume_owned_hitl(export, session, authority)).await?;
                Ok(())
            }
            Ok(RecoveryAction::Settle) => {
                Box::pin(self.recover_admission(&session, &mut authority)).await?;
                authority.release(&self.store, session.storage_key()).await
            }
            Ok(RecoveryAction::Parked) => {
                authority.release(&self.store, session.storage_key()).await
            }
            Err(error) => {
                // Uncertain publication retains its owner/envelope for lease-expiry
                // recovery. Inconsistent facts without an outbox need no owner.
                if authority
                    .context
                    .document
                    .state
                    .active
                    .as_ref()
                    .is_some_and(|a| a.publication.pending.is_none())
                {
                    authority
                        .release(&self.store, session.storage_key())
                        .await?;
                }
                #[cfg(feature = "fault-injection")]
                self.hooks
                    .checkpoint(crate::fault_injection::Boundary::HitlRecoveryDeferred)
                    .await;
                Err(error)
            }
        }
    }

    async fn recover_hitl_progress(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
    ) -> Result<RecoveryAction> {
        authority
            .flush_pending(&self.store, session.storage_key())
            .await?;
        self.repair_hitl_mapping(session, authority).await?;
        // Cancellation and durable terminal ordering retain the existing stop path.
        if authority
            .context
            .document
            .state
            .active
            .as_ref()
            .is_none_or(|a| a.cancel.is_some() || a.snapshot.task.status.state.is_terminal())
        {
            return Ok(RecoveryAction::Settle);
        }
        let ticket = authority.ticket(session.storage_key())?;
        if !matches!(
            session.resolve_fixed_admission(&ticket).await?,
            FixedAdmissionOutcome::Admitted { .. }
        ) {
            anyhow::ensure!(
                authority
                    .context
                    .document
                    .state
                    .active
                    .as_ref()
                    .is_some_and(|a| a.hitl_checkpoint.is_none()),
                "HITL checkpoint without admitted prompt"
            );
            return Ok(RecoveryAction::Settle);
        }
        // One delivery may race completion or the next approval. Always reclassify,
        // never rebuild accepted intent from the new current request.
        for _ in 0..3 {
            match session.fixed_prompt_progress(&ticket).await? {
                PromptProgress::Terminal(_) => {
                    return Ok(RecoveryAction::Settle);
                }
                PromptProgress::InputRequired(pending) => {
                    let active = authority
                        .context
                        .document
                        .state
                        .active
                        .as_ref()
                        .context("recovery task missing")?;
                    let request = pending.approval_ref(ticket.invocation_id());
                    if let Some(checkpoint) = &active.hitl_checkpoint {
                        anyhow::ensure!(
                            checkpoint.request == request
                                || checkpoint.request.request_seq < request.request_seq,
                            "HITL runtime request regressed or changed identity"
                        );
                        if checkpoint.request == request {
                            if let Some(receipt) = &checkpoint.reply {
                                anyhow::ensure!(
                                    checkpoint.phase == HitlPhase::DecisionIntent,
                                    "accepted HITL request is not DecisionIntent"
                                );
                                let note = saved_reply_note(active, receipt)?;
                                session
                                    .decide_hitl_approval_for(
                                        &receipt.request,
                                        receipt.approved,
                                        note,
                                    )
                                    .await?;
                                continue;
                            }
                            anyhow::ensure!(
                                checkpoint.phase == HitlPhase::Waiting,
                                "HITL Following request still undecided"
                            );
                        }
                    }
                    self.park_recovered_hitl(session, authority, &pending)
                        .await?;
                    return Ok(RecoveryAction::Parked);
                }
                PromptProgress::Running {
                    last_decision: Some(decision),
                } => {
                    self.follow_recovered_hitl(session, authority, &decision)
                        .await?;
                    return Ok(RecoveryAction::Follow);
                }
                PromptProgress::Running {
                    last_decision: None,
                } => {
                    anyhow::ensure!(
                        authority
                            .context
                            .document
                            .state
                            .active
                            .as_ref()
                            .is_some_and(|a| a.hitl_checkpoint.is_none()),
                        "HITL checkpoint has no runtime request or decision"
                    );
                    return Ok(RecoveryAction::Settle);
                }
            }
        }
        anyhow::bail!("HITL progress did not stabilize after decision delivery")
    }

    pub(in crate::runner) async fn repair_hitl_mapping(
        &self,
        session: &NatsSession,
        authority: &OwnedTask,
    ) -> Result<()> {
        let active = authority
            .context
            .document
            .state
            .active
            .as_ref()
            .context("recovery task missing")?;
        if let Some(reply) = active
            .hitl_checkpoint
            .as_ref()
            .and_then(|c| c.reply.as_ref())
        {
            self.store
                .put_reply_dedupe(
                    session.storage_key(),
                    &reply.message_id,
                    &active.snapshot.task.id,
                    &reply.fingerprint,
                )
                .await?;
        }
        Ok(())
    }

    async fn park_recovered_hitl(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        pending: &PendingHitlApproval,
    ) -> Result<()> {
        let request =
            pending.approval_ref(authority.ticket(session.storage_key())?.invocation_id());
        let active = authority
            .context
            .document
            .state
            .active
            .as_ref()
            .context("recovery task missing")?;
        if active.snapshot.task.status.state == TaskState::InputRequired
            && active
                .hitl_checkpoint
                .as_ref()
                .is_some_and(|c| c.request == request && c.phase == HitlPhase::Waiting)
        {
            return Ok(());
        }
        authority
            .update(&self.store, session.storage_key(), |active| {
                active.snapshot.task.status = status(
                    TaskState::InputRequired,
                    Some(&crate::hitl::render_challenge(pending)),
                );
                active.hitl_checkpoint = Some(HitlCheckpoint {
                    request: request.clone(),
                    phase: HitlPhase::Waiting,
                    reply: None,
                    observed_log_seq: pending.request_seq,
                });
                stage_recovered_status(active, pending.request_seq, "input-required");
            })
            .await?;
        authority
            .flush_pending(&self.store, session.storage_key())
            .await?;
        Ok(())
    }

    async fn follow_recovered_hitl(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        decision: &HitlDecisionReceipt,
    ) -> Result<()> {
        let active = authority
            .context
            .document
            .state
            .active
            .as_ref()
            .context("recovery task missing")?;
        if let Some(checkpoint) = &active.hitl_checkpoint {
            anyhow::ensure!(
                checkpoint.request == decision.request
                    || checkpoint.request.request_seq < decision.request.request_seq,
                "HITL decision predates checkpoint or changed identity"
            );
            if checkpoint.request == decision.request
                && checkpoint.phase == HitlPhase::Following
                && active.snapshot.task.status.state == TaskState::Working
            {
                // The typed follower activates only this ticket's original prompt.
                return Ok(());
            }
        }
        authority
            .update(&self.store, session.storage_key(), |active| {
                let checkpoint = active
                    .hitl_checkpoint
                    .get_or_insert_with(|| HitlCheckpoint {
                        request: decision.request.clone(),
                        phase: HitlPhase::Following,
                        reply: None,
                        observed_log_seq: decision.decision_seq,
                    });
                if checkpoint.request != decision.request {
                    *checkpoint = HitlCheckpoint {
                        request: decision.request.clone(),
                        phase: HitlPhase::Following,
                        reply: None,
                        observed_log_seq: decision.decision_seq,
                    };
                }
                checkpoint.phase = HitlPhase::Following;
                checkpoint.observed_log_seq = decision.decision_seq;
                if let Some(reply) = checkpoint.reply.as_mut() {
                    reply.outcome = Some(
                        if reply.approved == decision.approved {
                            "already_applied"
                        } else {
                            "conflicting"
                        }
                        .into(),
                    );
                }
                active.snapshot.task.status = status(TaskState::Working, None);
                stage_recovered_status(active, decision.request.request_seq, "working");
            })
            .await?;
        authority
            .flush_pending(&self.store, session.storage_key())
            .await?;
        Ok(())
    }
}

fn stage_recovered_status(active: &mut ActiveTask, request_seq: u64, state: &str) {
    let id = format!("{}:hitl:{request_seq}:{state}", active.snapshot.task.id);
    super::super::outbox::stage_event(
        active,
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: active.snapshot.task.id.clone(),
            context_id: active.snapshot.task.context_id.clone(),
            status: active.snapshot.task.status.clone(),
            metadata: None,
        }),
        &id,
    );
}

fn saved_reply_note(active: &ActiveTask, receipt: &ReplyReceipt) -> Result<Option<String>> {
    let saved = active
        .snapshot
        .task
        .history
        .as_ref()
        .and_then(|history| history.iter().find(|m| m.message_id == receipt.message_id))
        .context("accepted reply history missing")?;
    anyhow::ensure!(
        crate::store::message_fingerprint(&saved.parts) == receipt.fingerprint,
        "accepted reply history fingerprint mismatch"
    );
    match parse_reply(&saved.parts) {
        ReplyParse::Approve(note) if receipt.approved => Ok(note),
        ReplyParse::Reject(note) if !receipt.approved => Ok(note),
        _ => anyhow::bail!("accepted reply history decision mismatch"),
    }
}
