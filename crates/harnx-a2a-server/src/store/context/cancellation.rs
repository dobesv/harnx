//! External writers may only add a monotonic, exact task/invocation intent.
use super::*;

impl A2aStore {
    /// Binding/ACL must be checked before this trusted coordination operation.
    /// A superseded receipt is resolved by stable intent, never runtime replay.
    pub async fn request_task_cancel(&self, storage: &str, task: &str) -> Result<()> {
        let operation = uuid::Uuid::new_v4().to_string();
        for _ in 0..8 {
            let current = self
                .read_context(storage)
                .await?
                .context("cancel authority missing")?;
            let active = current
                .document
                .state
                .active
                .as_ref()
                .context(AuthorityError::TaskMismatch)?;
            ensure!(
                active.snapshot.task.id == task,
                AuthorityError::TaskMismatch
            );
            if active.snapshot.task.status.state.is_terminal() || active.cancel.is_some() {
                return Ok(());
            }
            let mut next = current.document.clone();
            next.state.active.as_mut().expect("checked active").cancel = Some(CancelIntent {
                requested_at: Some(chrono::Utc::now()),
                task_id: task.into(),
                invocation_id: active.admission.invocation_id.clone(),
                operation_id: operation.clone(),
            });
            let write = ContextWrite::new(storage, current.revision, next, &operation)?;
            match self.commit_context(&write).await {
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
