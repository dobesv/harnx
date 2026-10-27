use a2a_lf::{StreamResponse, TaskState, TaskStatusUpdateEvent};
use anyhow::Result;
use harnx_runtime::{HitlDecisionOutcome, NatsSession};

use super::target::ReplyTarget;
use crate::{
    runner::{authority::OwnedTask, event_map::status, Runner},
    store::context::{AuthorityError, HitlAdvanceParams, HitlPhase},
};

pub(super) async fn advance_reply(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
    advance: HitlAdvanceParams<'_>,
) -> Result<()> {
    for _ in 0..8 {
        authority
            .flush_pending(&runner.store, session.storage_key())
            .await?;
        match runner
            .store
            .advance_hitl_to_following(
                session.storage_key(),
                &authority.context.version()?,
                advance.clone(),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(error)
                if error.downcast_ref::<AuthorityError>() == Some(&AuthorityError::Conflict) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    anyhow::bail!(AuthorityError::Conflict)
}

pub(super) async fn transition_following_unresolved(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
    outcome: &HitlDecisionOutcome,
) -> Result<()> {
    authority
        .update(&runner.store, target.session.storage_key(), |active| {
            let checkpoint = active.hitl_checkpoint.as_mut().expect("checked checkpoint");
            assert_eq!(checkpoint.request, *target.request);
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
                active.snapshot.task.id, target.request.request_seq
            );
            crate::runner::outbox::stage_event(
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
    Ok(())
}
