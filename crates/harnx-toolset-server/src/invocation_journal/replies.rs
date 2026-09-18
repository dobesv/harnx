//! Reply projection and checkpoint storage for one journal row.
use super::*;

impl InvocationJournal {
    /// The first reply projected into the journal row wins by revision CAS, so
    /// a handler that finishes after the call was already answered gets the
    /// durable reply back instead of overwriting it.
    pub async fn complete(&self, request: &ToolRequest, reply: ToolReply) -> Result<ToolReply> {
        self.validate_replay(request).await?;
        ensure!(reply.call_id == request.call_id, "reply call ID mismatch");
        self.first_value(key(request), reply, |record| &mut record.reply)
            .await
    }

    pub async fn completed_reply(&self, request: &ToolRequest) -> Result<Option<ToolReply>> {
        self.validate_replay(request).await?;
        self.check_session_retained(request).await?;
        Ok(self.get(request).await?.and_then(|record| record.reply))
    }
}

/// Records one call's checkpoint handle in its own journal row, so
/// [`harnx_toolset::Toolset::cancel`] can act on it once the invocation that
/// wrote it is gone.
pub struct JournalCheckpointStore {
    pub journal: InvocationJournal,
    pub session: String,
    pub call_id: String,
}

#[async_trait::async_trait]
impl harnx_toolset::CheckpointStore for JournalCheckpointStore {
    async fn checkpoint(&self, value: serde_json::Value) -> Result<serde_json::Value> {
        self.journal
            .checkpoint(&self.session, &self.call_id, value)
            .await
    }
}
