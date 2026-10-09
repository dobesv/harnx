//! Recovery publishes final replacement text before committing terminal status.
use super::*;
use crate::runner::outbox::stage_event;

pub(super) async fn recover_artifact(
    authority: &mut OwnedTask,
    store: &A2aStore,
    session: &NatsSession,
    text: &str,
) -> Result<()> {
    let record = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("recovery artifact missing task")?
        .snapshot
        .clone();
    let response = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: record.task.id,
        context_id: record.task.context_id,
        artifact: artifact(text.into()),
        append: Some(false),
        last_chunk: Some(true),
        metadata: None,
    });
    let commit = uuid::Uuid::new_v4().to_string();
    authority
        .update(store, session.storage_key(), |active| {
            if active
                .publication
                .pending
                .as_ref()
                .is_some_and(|pending| pending.commit_id == commit)
            {
                return;
            }
            active.snapshot.task.artifacts = Some(vec![artifact(text.into())]);
            stage_event(active, response.clone(), &commit);
        })
        .await?;
    authority
        .flush_pending(store, session.storage_key())
        .await?;
    Ok(())
}
