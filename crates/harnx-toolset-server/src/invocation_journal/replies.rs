use super::*;
use crate::reply_fence;
use harnx_execution_control::{
    CommitAction, CommittedAction, CommittedOutput, ExecutionStore, GateAction, OutputKind,
};

impl InvocationJournal {
    fn execution(&self) -> Result<&ExecutionStore> {
        self.1
            .as_ref()
            .context("journal opened without execution authority")
    }

    /// The gate's reply slot wins, not a journal-row race. The immutable payload
    /// is staged first; a crash after gate CAS is recoverable without projection.
    pub async fn complete(&self, request: &ToolRequest, reply: ToolReply) -> Result<ToolReply> {
        self.complete_from(request, &reply_fence::identity(request)?.producer, reply)
            .await
    }

    pub(crate) async fn complete_from(
        &self,
        request: &ToolRequest,
        producer: &harnx_execution_control::ExecutionContext,
        reply: ToolReply,
    ) -> Result<ToolReply> {
        self.validate_replay(request).await?;
        let execution = reply_fence::identity(request)?;
        reply_fence::check_stop(self.execution()?, execution).await?;
        ensure!(
            producer.operation() == execution.producer.operation()
                && producer.generation() == execution.producer.generation()
                && producer.gate_root() == execution.producer.gate_root(),
            "reply attempt identity changed"
        );
        ensure!(reply.call_id == request.call_id, "reply call ID mismatch");
        if let Some(saved) = self.committed_reply(request).await? {
            self.project_reply(request, &saved).await?;
            return Ok(saved.reply);
        }
        let payload = reply_fence::payload(&reply)?;
        let blob_key = blob_key(request, &payload)?;
        let bytes = serde_json::to_vec(&reply)?;
        if let Err(error) = self.0.create(&blob_key, bytes.clone().into()).await {
            ensure!(
                self.0
                    .get(&blob_key)
                    .await?
                    .is_some_and(|saved| saved.as_ref() == bytes),
                "stage reply candidate: {error}"
            );
        }
        let action = CommitAction {
            id: format!(
                "reply-{}",
                payload["reply_sha256"]
                    .as_str()
                    .context("reply digest missing")?
            ),
            kind: GateAction::CommitOutput {
                output: CommittedOutput {
                    id: "reply".into(),
                    kind: OutputKind::ToolReply,
                    payload,
                },
            },
        };
        let result = self
            .execution()?
            .commit_if_admissible(producer, action)
            .await;
        // A concurrent committed reply or lost acknowledgement can win. Historical
        // lookup never overrides interruption; consumption is a separate CAS.
        reply_fence::check_stop(self.execution()?, execution).await?;
        let saved = self.committed_reply(request).await?;
        if let Some(saved) = saved {
            self.project_reply(request, &saved).await?;
            return Ok(saved.reply);
        }
        result?;
        anyhow::bail!("committed reply slot missing")
    }

    pub async fn committed_reply(&self, request: &ToolRequest) -> Result<Option<CommittedReply>> {
        let execution = reply_fence::identity(request)?;
        self.check_session_retained(request).await?;
        let record = self
            .get(request)
            .await?
            .context("durable invocation missing")?;
        let Some(decision) = self
            .execution()?
            .committed_tool_reply(&execution.producer)
            .await?
        else {
            ensure!(
                record.reply.is_none(),
                "legacy reply has no committed proof"
            );
            return Ok(None);
        };
        let CommittedAction::Action {
            action:
                CommitAction {
                    kind: GateAction::CommitOutput { output },
                    ..
                },
        } = decision.action
        else {
            anyhow::bail!("invalid reply commit");
        };
        let bytes = self
            .0
            .get(blob_key(request, &output.payload)?)
            .await?
            .context("committed reply payload missing")?;
        let saved = CommittedReply {
            producer: decision.context,
            receipt: decision.receipt,
            reply: serde_json::from_slice(&bytes)?,
        };
        reply_fence::verify(self.execution()?, &saved).await?;
        if let Some(producer) = record.reply_producer {
            ensure!(
                producer == saved.producer,
                "journal reply producer mismatch"
            );
        }
        if let Some(proof) = record.reply_commit {
            ensure!(
                proof == saved.receipt && record.reply.as_ref() == Some(&saved.reply),
                "journal projection does not match committed reply"
            );
        }
        Ok(Some(saved))
    }

    /// Resolve stop/lineage BEFORE the saved-reply branch, including when the
    /// physical operation has been retired. Unknown legacy authority fails closed.
    pub async fn completed_reply(&self, request: &ToolRequest) -> Result<Option<ToolReply>> {
        self.validate_replay(request).await?;
        reply_fence::check_request_stop(self.execution()?, request).await?;
        let execution = reply_fence::identity(request)?;
        reply_fence::check_stop(self.execution()?, execution).await?;
        match self.committed_reply(request).await? {
            Some(saved) => Ok(Some(
                reply_fence::consume(self.execution()?, request, &saved).await?,
            )),
            None => Ok(None),
        }
    }

    async fn project_reply(&self, request: &ToolRequest, saved: &CommittedReply) -> Result<()> {
        while !self.project_reply_attempt(request, saved).await? {}
        Ok(())
    }

    async fn project_reply_attempt(
        &self,
        request: &ToolRequest,
        saved: &CommittedReply,
    ) -> Result<bool> {
        let entry = self
            .0
            .entry(key(request))
            .await?
            .context("durable invocation missing")?;
        ensure!(entry.operation == kv::Operation::Put, "invocation deleted");
        let mut record: RecordedInvocation = serde_json::from_slice(&entry.value)?;
        if let Some(proof) = &record.reply_commit {
            ensure!(
                *proof == saved.receipt && record.reply.as_ref() == Some(&saved.reply),
                "reply projection conflict"
            );
            return Ok(true);
        }
        record.reply = Some(saved.reply.clone());
        record.reply_commit = Some(saved.receipt.clone());
        record.reply_producer = Some(saved.producer.clone());
        match self
            .0
            .update(
                key(request),
                serde_json::to_vec(&record)?.into(),
                entry.revision,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

fn blob_key(request: &ToolRequest, payload: &serde_json::Value) -> Result<String> {
    let digest = payload["reply_sha256"]
        .as_str()
        .context("reply digest missing")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid reply digest"
    );
    // Keep blobs inside session retention, but outside the invocation-row prefix.
    Ok(format!("blobs/{}/{digest}", key(request)))
}
