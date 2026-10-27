//! One committed envelope at a time; publication precedes cursor checkpoint.
use super::*;
use crate::store::context::ActiveTask;

pub(super) fn stage_event(active: &mut ActiveTask, response: StreamResponse, commit_id: &str) {
    active
        .stage_event(response, commit_id)
        .expect("event staging");
}

impl OwnedTask {
    async fn sync_context_for_flush(
        &mut self,
        store: &A2aStore,
        storage: &str,
    ) -> Result<Option<crate::store::context::PendingEvent>> {
        let current = store
            .read_context(storage)
            .await?
            .context("event authority missing")?;
        let expected = self.context.version()?;
        let actual = current.version()?;
        anyhow::ensure!(
            actual.owner == expected.owner && actual.task_id == expected.task_id,
            crate::store::context::AuthorityError::StaleOwner
        );
        self.context = current;
        let active = self
            .context
            .document
            .state
            .active
            .as_ref()
            .context("event task missing")?;
        let Some(pending) = active.publication.pending.clone() else {
            return Ok(None);
        };
        #[cfg(feature = "fault-injection")]
        fault_committed(store, active).await;
        Ok(Some(pending))
    }

    pub(super) async fn flush_pending(
        &mut self,
        store: &A2aStore,
        storage: &str,
    ) -> Result<Option<A2aEvent>> {
        let Some(pending) = self.sync_context_for_flush(store, storage).await? else {
            return Ok(None);
        };
        store.cleanup_context_events(storage).await?;
        let sequence =
            crate::store::context::terminal_event::publish_pending(store, &self.context, storage)
                .await?;
        self.update(store, storage, |active| {
            clear_published_pending(active, &pending.commit_id, sequence);
        })
        .await?;
        #[cfg(feature = "fault-injection")]
        store
            .context_fault_hooks()
            .checkpoint(crate::fault_injection::Boundary::OutboxCleared)
            .await;
        Ok(Some(A2aEvent {
            sequence: pending.task_sequence,
            response: pending.response,
        }))
    }
}

fn clear_published_pending(active: &mut ActiveTask, pending_commit_id: &str, sequence: u64) {
    if active.publication.pending.is_none() {
        assert_eq!(active.publication.subject_sequence, sequence);
        return;
    }
    assert_eq!(
        active
            .publication
            .pending
            .as_ref()
            .map(|p| p.commit_id.as_str()),
        Some(pending_commit_id)
    );
    active.publication.pending = None;
    active.publication.subject_sequence = sequence;
    active.publication.confirmed_history.push(sequence);
    if active.publication.confirmed_history.len() > harnx_runtime::a2a_events::CHECKPOINT_HISTORY {
        active.publication.confirmed_history.remove(0);
    }
    if active.snapshot.task.status.state.is_terminal() {
        active.projections.final_event = true;
    }
}

#[cfg(feature = "fault-injection")]
async fn fault_committed(store: &A2aStore, active: &ActiveTask) {
    use crate::fault_injection::Boundary;
    let hooks = store.context_fault_hooks();
    hooks.checkpoint(Boundary::OutboxCommitted).await;
    if active.snapshot.task.status.state == TaskState::InputRequired {
        hooks.checkpoint(Boundary::HitlOutboxCommitted).await;
    }
    if active.snapshot.task.status.state.is_terminal() {
        hooks.checkpoint(Boundary::TerminalOutboxCommitted).await;
    }
}
