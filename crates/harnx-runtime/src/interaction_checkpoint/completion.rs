use super::{live::DurableRound, snapshot::ResultSnapshot};
use crate::{
    client::MessageContent,
    config::session_tool_results::{completion_media_location, prepare_completed_observation},
    nats_attachments::{externalize_message_attachments, AttachmentLocation},
};
use anyhow::{ensure, Context, Result};
use harnx_core::{
    session::{SessionLogEntry, ToolOutput},
    tool::ToolResult,
};
use harnx_engine::tool::{InnerToolCallStatus, MaterializedToolResult, ToolEvalContext};

impl DurableRound {
    /// Loop-guard refusals bypass engine evaluation but still own a final slot.
    pub(crate) async fn complete_refused(
        &self,
        refused: &[ToolResult],
        render: &ToolEvalContext,
    ) -> Result<()> {
        if refused.is_empty() {
            return Ok(());
        }
        let completed = self.completed().await?;
        for result in refused {
            if !completed.iter().any(|old| old.call.id == result.call.id) {
                self.complete(result.clone(), render).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn complete(
        &self,
        result: ToolResult,
        render: &ToolEvalContext,
    ) -> Result<ToolResult> {
        self.ensure_started().await?;
        // Keep the persisted snapshot future off the tool driver's poll stack.
        Box::pin(self.complete_recovered(result, render)).await
    }

    // Reconciliation must not claim that an older round never dispatched.
    // Persist its classified slots before a fresh attempt writes round-start.
    pub(crate) async fn complete_recovered(
        &self,
        result: ToolResult,
        render: &ToolEvalContext,
    ) -> Result<ToolResult> {
        let mut result = crate::tool::populate_result_markdown(vec![result], render)
            .pop()
            .context("missing finalized tool result")?;
        self.externalize(&mut result).await?;
        let observation = self.observation(&result)?;
        let snapshot = ResultSnapshot::new(&MaterializedToolResult::new(
            result.clone(),
            InnerToolCallStatus::Ok,
        ));
        let reference = self
            .store
            .put(
                &self.prefix(&result.call)?,
                &format!("completions.{}", uuid::Uuid::new_v4()),
                &snapshot,
            )
            .await?;
        crate::fault_injection::hit(
            self.session.backend.session_id(),
            crate::fault_injection::RecoveryFaultPoint::OuterCompletionRecordBefore,
        )
        .await;
        self.session
            .append(&SessionLogEntry::ToolCallCompleted {
                tool_round_seq: self.seq,
                tool_call_id: result
                    .call
                    .id
                    .clone()
                    .context("missing completed call ID")?,
                result: public_output(&result),
                private_snapshot: reference,
                fence_token: self.session.lease.fence_token(),
            })
            .await?;
        crate::fault_injection::hit(
            self.session.backend.session_id(),
            crate::fault_injection::RecoveryFaultPoint::OuterCompletionRecordAfter,
        )
        .await;
        // Projection failure is retryable from the snapshot, never by re-dispatch.
        observation.persist().await;
        Ok(result)
    }

    pub(super) async fn completed(&self) -> Result<Vec<ToolResult>> {
        let entries = self.session.backend.load_events_latest_async().await?;
        let mut completed = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Recovery may replace a completion whose private data was lost with an
        // interrupted result. Only the newest durable slot is authoritative.
        for (_, entry) in entries.into_iter().rev() {
            let SessionLogEntry::ToolCallCompleted {
                tool_round_seq,
                tool_call_id,
                result: public,
                private_snapshot,
                ..
            } = entry
            else {
                continue;
            };
            if tool_round_seq != self.seq || !seen.insert(tool_call_id.clone()) {
                continue;
            }
            let call = self
                .calls
                .iter()
                .find(|call| call.id.as_deref() == Some(tool_call_id.as_str()))
                .context("completed call is not in owning round")?;
            let snapshot: ResultSnapshot = self
                .store
                .read(&self.prefix(call)?, &private_snapshot)
                .await?;
            let result = snapshot.restore()?.result;
            ensure!(
                result.call == *call && public_output(&result) == public,
                "completed tool snapshot identity mismatch"
            );
            self.observation(&result)?.persist().await;
            completed.push(result);
        }
        Ok(completed)
    }

    fn observation(
        &self,
        result: &ToolResult,
    ) -> Result<crate::config::session_persistence::PendingExecutionContextPersistence> {
        let config = self.config.read();
        let session = config
            .session
            .as_ref()
            .context("completed call has no session")?;
        prepare_completed_observation(session, result)
    }

    pub(super) async fn externalize(&self, result: &mut ToolResult) -> Result<()> {
        let (session_ref, source_dir) = {
            let config = self.config.read();
            completion_media_location(
                config
                    .session
                    .as_ref()
                    .context("tool media has no session")?,
            )?
        };
        let js = self.session.backend.jetstream();
        let mut content = MessageContent::Array(std::mem::take(&mut result.content));
        externalize_message_attachments(
            AttachmentLocation::new(&js, self.session.replicas, &session_ref),
            &mut content,
            source_dir.as_deref(),
        )
        .await?;
        result.content = media_parts(content)?;
        Ok(())
    }
}

// qual:allow(dry, boilerplate) reason: "Project only public fields from a borrowed private snapshot; cloning the whole result would include observations."
pub(crate) fn public_output(result: &ToolResult) -> ToolOutput {
    ToolOutput {
        id: result.call.id.clone(),
        name: result.call.name.clone(),
        output: result.output.clone(),
        markdown: result.markdown.clone(),
        content: result.content.clone(),
        switch_agent: result.switch_agent.clone(),
    }
}

fn media_parts(content: MessageContent) -> Result<Vec<harnx_core::message::MessageContentPart>> {
    match content {
        MessageContent::Array(parts) => Ok(parts),
        _ => anyhow::bail!("externalized tool media changed its content kind"),
    }
}
