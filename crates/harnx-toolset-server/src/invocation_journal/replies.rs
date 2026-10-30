//! Reply projection, checkpoint and partial-result storage for one journal row.
use super::*;

impl InvocationJournal {
    pub async fn completed_reply(&self, request: &ToolRequest) -> Result<Option<ToolReply>> {
        Ok(self
            .completed_record(request)
            .await?
            .and_then(|record| record.reply))
    }

    pub(crate) async fn completed_reply_for_delivery(
        &self,
        request: &ToolRequest,
    ) -> Result<Option<ToolReply>> {
        Ok(self
            .completed_record(request)
            .await?
            .and_then(|record| completion::recorded_reply(record, true)))
    }

    /// Read the latest row after validating replay identity and session retention.
    /// The row may still be awaiting a reply. Reply and partial result in a
    /// returned row come from the same revision.
    pub async fn completed_record(
        &self,
        request: &ToolRequest,
    ) -> Result<Option<RecordedInvocation>> {
        self.validate_replay(request).await?;
        self.check_session_retained(request).await?;
        self.get(request).await
    }
}

/// Records one call's checkpoint handle in its own journal row, so
/// [`harnx_toolset::Toolset::cancel`] can act on it once the invocation that
/// wrote it is gone.
pub struct JournalCheckpointStore {
    journal: InvocationJournal,
    key: String,
}

#[async_trait::async_trait]
impl harnx_toolset::CheckpointStore for JournalCheckpointStore {
    /// Publish a durable job handle before starting work. Concurrent replay
    /// observers all receive the first handle; no second job may be started.
    async fn checkpoint(&self, value: serde_json::Value) -> Result<serde_json::Value> {
        self.journal
            .first_value(self.key.clone(), value, |record| &mut record.checkpoint)
            .await
    }
}

impl InvocationJournal {
    /// The store a running call records its checkpoint through, keyed like
    /// the call's own row.
    pub fn checkpoint_store(&self, request: &ToolRequest) -> JournalCheckpointStore {
        JournalCheckpointStore {
            journal: self.clone(),
            key: key(request),
        }
    }

    /// The store a running call records its partial result through. It is
    /// keyed like the call's own row, so a call with no parent session writes
    /// to its standalone row.
    pub fn partial_result_store(&self, request: &ToolRequest) -> JournalPartialResultStore {
        JournalPartialResultStore {
            journal: self.clone(),
            key: key(request),
        }
    }

    async fn replace_partial_result(&self, key: &str, value: serde_json::Value) -> Result<()> {
        while !self.try_replace_partial_result(key, &value).await? {}
        Ok(())
    }

    /// One compare-and-set attempt; `false` means the row changed underneath
    /// it. A recorded reply already decided the call, so a partial result
    /// arriving after it is dropped rather than describing a call that is over.
    async fn try_replace_partial_result(
        &self,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<bool> {
        Ok(self
            .try_update_record(key, "persist tool invocation partial result", |record| {
                if record.reply.is_some() || record.partial_result.as_ref() == Some(value) {
                    return RecordUpdate::Unchanged(());
                }
                record.partial_result = Some(value.clone());
                RecordUpdate::Changed(())
            })
            .await?
            .is_some())
    }
}

/// Records one call's partial result in its own journal row, where wind-up,
/// replay and a caller whose call failed all read it.
pub struct JournalPartialResultStore {
    journal: InvocationJournal,
    key: String,
}

#[async_trait::async_trait]
impl harnx_toolset::PartialResultStore for JournalPartialResultStore {
    async fn record_partial_result(&self, value: serde_json::Value) -> Result<()> {
        self.journal.replace_partial_result(&self.key, value).await
    }
}
