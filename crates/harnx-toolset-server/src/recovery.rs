//! Replay validation and durable results are independent of the live reply cache.
use super::*;
use invocation_journal::InvocationJournal;

pub(super) async fn validate_replay(
    context: &ToolRequestContext,
    request: &ToolRequest,
) -> Result<()> {
    let journal =
        invocation_journal::InvocationJournal::ensure(&jetstream::new(context.client.clone()))
            .await?;
    journal.check_session_retained(request).await?;
    if request.replay.is_some() || journal.get(request).await?.is_some() {
        journal.validate_replay(request).await?;
    }
    let Some(owner) = &request.replay else {
        return Ok(());
    };
    let session = request
        .parent_session_id
        .as_deref()
        .context("replay requires a parent session")?;
    let parent = context
        .execution_store
        .current(session)
        .await?
        .context("replay parent missing")?;
    parent.check_owner(owner)?;
    context
        .execution_store
        .check_ancestors(&parent.reference)
        .await
}

pub(super) struct InvocationRecovery {
    request: ToolRequest,
    journal: InvocationJournal,
    saved_reply: Option<ToolReply>,
    recorded: bool,
}

impl InvocationRecovery {
    pub async fn load(
        context: &ToolRequestContext,
        request: &ToolRequest,
    ) -> Result<Self, ToolInvokeError> {
        let journal = InvocationJournal::ensure(&jetstream::new(context.client.clone()))
            .await
            .map_err(journal_error)?;
        let record = journal.get(request).await.map_err(journal_error)?;
        if record.is_some() {
            journal
                .validate_replay(request)
                .await
                .map_err(journal_error)?;
        }
        let recorded = record.is_some();
        let saved_reply = record.and_then(|record| record.reply);
        Ok(Self {
            request: request.clone(),
            journal,
            saved_reply,
            recorded,
        })
    }

    pub async fn completed_reply(
        &self,
        context: &ToolRequestContext,
    ) -> Result<Option<ToolReply>, ToolInvokeError> {
        let Some(reply) = &self.saved_reply else {
            return Ok(None);
        };
        let reference = harnx_execution_control::OperationRef::new(
            self.request
                .parent_session_id
                .as_deref()
                .unwrap_or(&self.request.call_id),
            &self.request.operation_id,
        );
        let operation = context
            .execution_store
            .get(&reference)
            .await
            .map_err(journal_error)?;
        Ok(operation
            .is_none_or(|op| op.state.is_terminal())
            .then(|| reply.clone()))
    }

    pub async fn check_policy(&self, context: &ToolRequestContext) -> Result<(), ToolInvokeError> {
        if self.request.replay.is_none() || self.saved_reply.is_some() {
            return Ok(());
        }
        if context.toolset.can_replay(&self.request.tool) {
            return Ok(());
        }
        // A request saved before dispatch may have no owner at all. Rejecting
        // it must close that unstarted registration, without declaring an old
        // handler or its unknown descendants stopped.
        execution::complete_without_invocation(context, &self.request)
            .await
            .map_err(journal_error)?;
        Err(ToolInvokeError::Recoverable("tool response lost (session was interrupted before results were persisted); this tool cannot replay the interrupted operation".into()))
    }

    pub async fn invoke(
        self,
        toolset: &dyn Toolset,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let result = match self.saved_reply {
            Some(reply) => reply_result(reply),
            None if self.request.replay.is_some() => toolset.replay(invocation).await,
            None => toolset.invoke_with_context(invocation).await,
        };
        if !self.recorded {
            // Direct tool clients do not participate in worker transcript recovery.
            return result;
        }
        // Persist before owner_stopped: the execution graph may be pruned before
        // the parent appends ToolResults, and an in-memory cache dies on restart.
        let reply = ToolReply {
            call_id: self.request.call_id.clone(),
            result: result.map_err(map_invoke_error),
        };
        reply_result(
            self.journal
                .complete(&self.request, reply)
                .await
                .map_err(journal_error)?,
        )
    }
}

fn journal_error(error: anyhow::Error) -> ToolInvokeError {
    ToolInvokeError::Fatal(format!("tool invocation recovery: {error:#}"))
}
