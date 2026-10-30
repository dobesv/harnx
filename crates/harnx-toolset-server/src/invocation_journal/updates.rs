//! Shared compare-and-set attempt for journal field updates.
use super::*;

pub(super) enum RecordUpdate<T> {
    Unchanged(T),
    Changed(T),
}

impl InvocationJournal {
    /// `None` means a conflicting write won; the caller must read and try again.
    /// An unchanged row returns its value without writing another revision.
    pub(super) async fn try_update_record<T>(
        &self,
        key: &str,
        error_context: &'static str,
        update: impl FnOnce(&mut RecordedInvocation) -> RecordUpdate<T>,
    ) -> Result<Option<T>> {
        let entry = self
            .entry(key)
            .await?
            .context("durable tool invocation missing")?;
        ensure!(
            entry.operation == kv::Operation::Put,
            "tool invocation was deleted"
        );
        let mut record: RecordedInvocation = serde_json::from_slice(&entry.value)?;
        let value = match update(&mut record) {
            RecordUpdate::Unchanged(value) => return Ok(Some(value)),
            RecordUpdate::Changed(value) => value,
        };
        match self
            .0
            .update(key, serde_json::to_vec(&record)?.into(), entry.revision)
            .await
        {
            Ok(_) => Ok(Some(value)),
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(None),
            Err(error) => Err(error).context(error_context),
        }
    }
}
