//! The journal row decides a call: it admits replays, holds the saved reply,
//! and its first recorded reply is the one every later attempt observes.
use super::*;
use invocation_journal::InvocationJournal;

pub(super) struct InvocationRecovery {
    request: ToolRequest,
    journal: InvocationJournal,
    /// The handle a previous attempt recorded, handed back to the tool so it
    /// can resume that work instead of starting it again.
    checkpoint: Option<Value>,
}

impl InvocationRecovery {
    pub async fn load(
        context: &ToolRequestContext,
        request: &ToolRequest,
    ) -> Result<Self, ToolInvokeError> {
        context
            .journal
            .validate_replay(request)
            .await
            .map_err(invoke_error)?;
        let recorded = context.journal.get(request).await.map_err(invoke_error)?;
        Ok(Self {
            request: request.clone(),
            journal: context.journal.clone(),
            checkpoint: recorded.and_then(|record| record.checkpoint),
        })
    }

    pub fn journal(&self) -> InvocationJournal {
        self.journal.clone()
    }

    pub fn checkpoint(&self) -> Option<Value> {
        self.checkpoint.clone()
    }

    pub async fn completed_reply(&self) -> Result<Option<ToolReply>, ToolInvokeError> {
        self.journal
            .completed_reply(&self.request)
            .await
            .map_err(invoke_error)
    }

    pub fn check_policy(&self, toolset: &dyn Toolset) -> Result<(), ToolInvokeError> {
        if self.request.replay.is_none() || toolset.can_replay(&self.request.tool) {
            return Ok(());
        }
        Err(ToolInvokeError::Recoverable(
            "tool response lost; this tool cannot replay the operation".into(),
        ))
    }

    /// Run the tool. Only the request path records a reply, and it records the
    /// one it is about to deliver, so a handler that finishes after its call
    /// was answered has nothing left to write and nothing to race.
    pub async fn invoke(
        self,
        toolset: &dyn Toolset,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        if self.request.replay.is_some() {
            toolset.replay(invocation).await
        } else {
            toolset.invoke_with_context(invocation).await
        }
    }
}

/// Journal and admission failures are this server's, not the tool's: the call
/// never produced a result, so the caller cannot treat one as returned.
pub(super) fn invoke_error(error: anyhow::Error) -> ToolInvokeError {
    ToolInvokeError::Fatal(format!("tool invocation recovery: {error:#}"))
}
