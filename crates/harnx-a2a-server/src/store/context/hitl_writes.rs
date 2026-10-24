//! HITL mutations share authority fencing and the runner's one-event outbox.
use super::*;
use crate::store::TaskRecord;
use a2a_lf::{Message, Part, Role, StreamResponse, TaskState, TaskStatusUpdateEvent};
use harnx_runtime::{ApprovalRef, HitlDecisionOutcome};

impl A2aStore {
    /// Commit Waiting + InputRequired and stage its status event atomically.
    /// Drain existing output first; after this write use OwnedTask::flush_pending
    /// before releasing ownership. This helper doesn't publish or release leases.
    pub async fn set_hitl_checkpoint_waiting(
        &self,
        storage_key: &str,
        expected: &ContextVersion,
        request: ApprovalRef,
        observed_log_seq: u64,
        challenge_message: &str,
    ) -> Result<TaskRecord> {
        let write = self
            .prepare_context_update_checked(
                storage_key,
                expected,
                &uuid::Uuid::new_v4().to_string(),
                |state| {
                    let active = state.active.as_mut().context("active task missing")?;
                    ensure!(
                        !active.snapshot.task.status.state.is_terminal(),
                        "task is terminal"
                    );
                    if let Some(previous) = &active.hitl_checkpoint {
                        ensure!(
                            request.request_seq > previous.request.request_seq,
                            "HITL request did not advance"
                        );
                        ensure!(
                            previous.phase == HitlPhase::Following,
                            "previous HITL request is unresolved"
                        );
                    }
                    let id = format!(
                        "{}:hitl:{}:input-required",
                        active.snapshot.task.id, request.request_seq
                    );
                    active.hitl_checkpoint = Some(HitlCheckpoint {
                        request,
                        phase: HitlPhase::Waiting,
                        reply: None,
                        observed_log_seq,
                    });
                    active.snapshot.task.status.state = TaskState::InputRequired;
                    active.snapshot.task.status.message = Some(Message::new(
                        Role::Agent,
                        vec![Part::text(challenge_message)],
                    ));
                    active.snapshot.task.status.timestamp = Some(chrono::Utc::now());
                    stage_status(active, &id)
                },
            )
            .await?;
        let committed = self.commit_context(&write).await?;
        Ok(committed
            .document
            .state
            .active
            .context("active task missing")?
            .snapshot)
    }

    /// Bind a reply to the exact current request. Missing/mismatched checkpoints,
    /// non-Waiting phases and competing replies error without committing a revision.
    /// Persist put_reply_dedupe next, then deliver this receipt's exact request.
    pub async fn record_hitl_reply_intent(
        &self,
        storage_key: &str,
        expected: &ContextVersion,
        expected_approval: &ApprovalRef,
        mut receipt: ReplyReceipt,
    ) -> Result<bool> {
        receipt.note = receipt.note.map(|note| hitl::truncate_note(&note));
        let write = self
            .prepare_context_update_checked(
                storage_key,
                expected,
                &uuid::Uuid::new_v4().to_string(),
                |state| {
                    let active = state.active.as_mut().context("active task missing")?;
                    ensure!(
                        active.snapshot.task.status.state == TaskState::InputRequired,
                        "task is not awaiting HITL input"
                    );
                    let checkpoint = matching_checkpoint(active, expected_approval)?;
                    ensure!(
                        receipt.request == *expected_approval,
                        "HITL reply request mismatch"
                    );
                    ensure!(
                        checkpoint.phase == HitlPhase::Waiting && checkpoint.reply.is_none(),
                        "HITL reply already accepted"
                    );
                    ensure!(
                        receipt.outcome.is_none(),
                        "new HITL reply already has outcome"
                    );
                    checkpoint.phase = HitlPhase::DecisionIntent;
                    checkpoint.reply = Some(receipt);
                    Ok(())
                },
            )
            .await?;
        self.commit_context(&write).await?;
        Ok(true)
    }

    /// Commit Following + Working and stage its status event. Drain the previous
    /// outbox first. Only Applied/AlreadyApplied may resume; Stale/Conflicting need
    /// a fresh runtime progress query instead. Retain checkpoint for recovery.
    pub async fn advance_hitl_to_following(
        &self,
        storage_key: &str,
        expected: &ContextVersion,
        expected_approval: &ApprovalRef,
        outcome: &HitlDecisionOutcome,
    ) -> Result<TaskRecord> {
        let write = self
            .prepare_context_update_checked(
                storage_key,
                expected,
                &uuid::Uuid::new_v4().to_string(),
                |state| {
                    let active = state.active.as_mut().context("active task missing")?;
                    ensure!(
                        active.snapshot.task.status.state == TaskState::InputRequired,
                        "task is not awaiting HITL input"
                    );
                    let checkpoint = matching_checkpoint(active, expected_approval)?;
                    ensure!(
                        checkpoint.phase != HitlPhase::Following,
                        "HITL request already following"
                    );
                    let label = match outcome {
                        HitlDecisionOutcome::Applied => "applied",
                        HitlDecisionOutcome::AlreadyApplied { approved, .. } => {
                            if let Some(reply) = &checkpoint.reply {
                                ensure!(
                                    *approved == reply.approved,
                                    "HITL decision conflicts with reply"
                                );
                            }
                            "already_applied"
                        }
                        _ => anyhow::bail!("HITL decision was not applied"),
                    };
                    if let Some(reply) = &mut checkpoint.reply {
                        reply.outcome = Some(label.into());
                    }
                    checkpoint.phase = HitlPhase::Following;
                    active.snapshot.task.status.state = TaskState::Working;
                    active.snapshot.task.status.message = None;
                    active.snapshot.task.status.timestamp = Some(chrono::Utc::now());
                    let id = format!(
                        "{}:hitl:{}:working",
                        active.snapshot.task.id, expected_approval.request_seq
                    );
                    stage_status(active, &id)
                },
            )
            .await?;
        let committed = self.commit_context(&write).await?;
        Ok(committed
            .document
            .state
            .active
            .context("active task missing")?
            .snapshot)
    }

    /// Clear only after terminal settlement. Central transition validation also
    /// requires any accepted reply mapping to be durable, including on retirement.
    pub async fn clear_hitl_checkpoint(
        &self,
        storage_key: &str,
        expected: &ContextVersion,
    ) -> Result<TaskRecord> {
        let write = self
            .prepare_context_update_checked(
                storage_key,
                expected,
                &uuid::Uuid::new_v4().to_string(),
                |state| {
                    let active = state.active.as_mut().context("active task missing")?;
                    ensure!(
                        active.snapshot.task.status.state.is_terminal(),
                        "task is not terminal"
                    );
                    ensure!(active.hitl_checkpoint.is_some(), "HITL checkpoint missing");
                    active.hitl_checkpoint = None;
                    Ok(())
                },
            )
            .await?;
        let committed = self.commit_context(&write).await?;
        Ok(committed
            .document
            .state
            .active
            .context("active task missing")?
            .snapshot)
    }

    pub(super) async fn verify_hitl_transition(
        &self,
        storage_key: &str,
        old: &ContextState,
        next: &ContextState,
    ) -> Result<()> {
        let Some(previous) = &old.active else {
            return Ok(());
        };
        let Some(checkpoint) = &previous.hitl_checkpoint else {
            return Ok(());
        };
        let next_active = next
            .active
            .as_ref()
            .filter(|a| a.snapshot.task.id == previous.snapshot.task.id);
        let next_checkpoint = next_active.and_then(|a| a.hitl_checkpoint.as_ref());
        if next_active.is_some() && next_checkpoint.is_none() {
            ensure!(
                next_active.is_some_and(|a| a.snapshot.task.status.state.is_terminal()),
                "non-terminal HITL checkpoint cannot be cleared"
            );
        }
        let Some(reply) = &checkpoint.reply else {
            return Ok(());
        };
        if let Some(next) = next_checkpoint.filter(|c| c.request == checkpoint.request) {
            // Same generation's accepted control intent is immutable, even after
            // dedupe is projected. Outcome alone can change after delivery.
            let next_reply = next
                .reply
                .as_ref()
                .context("accepted HITL reply cannot be cleared")?;
            let mut accepted = reply.clone();
            accepted.outcome = next_reply.outcome.clone();
            ensure!(accepted == *next_reply, "accepted HITL reply is immutable");
            return Ok(());
        }
        let mapping = self
            .get_reply_dedupe(storage_key, &reply.message_id)
            .await?;
        ensure!(
            mapping == Some((previous.snapshot.task.id.clone(), reply.fingerprint.clone())),
            "HITL reply mapping is not durable"
        );
        Ok(())
    }
}

fn matching_checkpoint<'a>(
    active: &'a mut ActiveTask,
    expected: &ApprovalRef,
) -> Result<&'a mut HitlCheckpoint> {
    let checkpoint = active
        .hitl_checkpoint
        .as_mut()
        .context("HITL checkpoint missing")?;
    ensure!(
        checkpoint.request == *expected,
        "HITL approval request mismatch"
    );
    if let Some(reply) = &checkpoint.reply {
        ensure!(reply.request == *expected, "HITL reply request mismatch");
    }
    Ok(checkpoint)
}

fn stage_status(active: &mut ActiveTask, id: &str) -> Result<()> {
    active.stage_event(
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: active.snapshot.task.id.clone(),
            context_id: active.snapshot.task.context_id.clone(),
            status: active.snapshot.task.status.clone(),
            metadata: None,
        }),
        id,
    )
}
