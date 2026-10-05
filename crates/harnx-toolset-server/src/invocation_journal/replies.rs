//! Reply projection, checkpoint and partial-result storage for one journal row.
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
        let entry = self
            .entry(key)
            .await?
            .context("durable tool invocation missing")?;
        ensure!(
            entry.operation == kv::Operation::Put,
            "tool invocation was deleted"
        );
        let mut record: RecordedInvocation = serde_json::from_slice(&entry.value)?;
        if record.reply.is_some() || record.partial_result.as_ref() == Some(value) {
            return Ok(true);
        }
        record.partial_result = Some(value.clone());
        match self
            .0
            .update(key, serde_json::to_vec(&record)?.into(), entry.revision)
            .await
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(false),
            Err(error) => Err(error).context("persist tool invocation partial result"),
        }
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
