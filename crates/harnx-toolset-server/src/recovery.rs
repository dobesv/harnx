//! Recovery uses original generation authority before either saved-reply or work admission.
use super::*;
use invocation_journal::InvocationJournal;

pub(super) async fn validate_replay(
    context: &ToolRequestContext,
    request: &ToolRequest,
) -> Result<()> {
    reply_fence::check_request_stop(&context.execution_store, request).await?;
    context.journal.check_session_retained(request).await?;
    context.journal.validate_replay(request).await?;
    let execution = reply_fence::identity(request)?;
    reply_fence::check_stop(&context.execution_store, execution).await?;
    if let Some(owner) = &request.replay {
        anyhow::ensure!(
            request.replay_execution.is_some(),
            "replay attempt identity missing"
        );
        // Exact original receiver, never whichever execution is current now.
        let parent = context
            .execution_store
            .get(execution.consumer.operation())
            .await?
            .context("replay parent missing")?;
        parent.check_owner(owner)?;
        anyhow::ensure!(
            execution.consumer.owner() == owner,
            "replay consumer owner mismatch"
        );
        context
            .execution_store
            .check_ancestors(&parent.reference)
            .await?;
    } else {
        anyhow::ensure!(
            request.replay_execution.is_none(),
            "replay identity without owner attestation"
        );
    }
    reply_fence::admit_recovery(&context.execution_store, request).await
}

pub(super) struct InvocationRecovery {
    request: ToolRequest,
    journal: InvocationJournal,
}

impl InvocationRecovery {
    pub async fn load(
        context: &ToolRequestContext,
        request: &ToolRequest,
    ) -> Result<Self, ToolInvokeError> {
        validate_replay(context, request)
            .await
            .map_err(reply_fence::invoke_error)?;
        Ok(Self {
            request: request.clone(),
            journal: context.journal.clone(),
        })
    }

    pub async fn completed_reply(
        &self,
        _context: &ToolRequestContext,
    ) -> Result<Option<ToolReply>, ToolInvokeError> {
        self.journal
            .completed_reply(&self.request)
            .await
            .map_err(reply_fence::invoke_error)
    }

    pub async fn check_policy(&self, context: &ToolRequestContext) -> Result<(), ToolInvokeError> {
        if self.request.replay.is_none() || context.toolset.can_replay(&self.request.tool) {
            return Ok(());
        }
        execution::complete_without_invocation(context, &self.request)
            .await
            .map_err(reply_fence::invoke_error)?;
        Err(ToolInvokeError::Recoverable(
            "tool response lost; this tool cannot replay the operation".into(),
        ))
    }

    pub async fn invoke(
        self,
        toolset: &dyn Toolset,
        invocation: ToolInvocation,
        producer: harnx_execution_control::ExecutionContext,
    ) -> Result<Value, ToolInvokeError> {
        let result = if self.request.replay.is_some() {
            toolset.replay(invocation).await
        } else {
            toolset.invoke_with_context(invocation).await
        };
        let reply = ToolReply {
            call_id: self.request.call_id.clone(),
            result: result.map_err(map_invoke_error),
        };
        reply_result(
            Box::pin(self.journal.complete_from(&self.request, &producer, reply))
                .await
                .map_err(reply_fence::invoke_error)?,
        )
    }
}

pub(super) fn reply_result(reply: ToolReply) -> Result<Value, ToolInvokeError> {
    reply.result.map_err(|error| match error {
        ToolErrorPayload::Recoverable(message) => ToolInvokeError::Recoverable(message),
        ToolErrorPayload::Fatal(message) => ToolInvokeError::Fatal(message),
        ToolErrorPayload::Interrupted(interrupted) => ToolInvokeError::Interrupted(interrupted),
    })
}
