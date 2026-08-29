//! Chat-completion persistence methods extracted from config/mod.rs for code health.
use super::*;

async fn await_context_persistence(
    persistence: Result<session_persistence::PendingExecutionContextPersistence>,
) -> Result<()> {
    persistence?.persist().await;
    Ok(())
}

impl Config {
    pub async fn after_chat_completion(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        tool_results: &[ToolResult],
        usage: &crate::client::CompletionTokenUsage,
    ) -> Result<()> {
        let request = SessionSaveRequest::new(input, output, thought);
        await_context_persistence(self.prepare_after_chat_completion(&request, tool_results, usage))
            .await
    }

    pub(crate) fn prepare_after_chat_completion(
        &mut self,
        request: &SessionSaveRequest<'_>,
        tool_results: &[ToolResult],
        usage: &crate::client::CompletionTokenUsage,
    ) -> Result<session_persistence::PendingExecutionContextPersistence> {
        self.record_completion_usage(usage);
        self.update_last_message_after_completion(request, tool_results);
        self.prepare_chat_completion_persistence(request, tool_results)
    }

    fn update_last_message_after_completion(
        &mut self,
        request: &SessionSaveRequest<'_>,
        tool_results: &[ToolResult],
    ) {
        if !tool_results.is_empty() {
            return;
        }

        self.last_message = Some(LastMessage::new(
            request.input.clone(),
            request.output.to_string(),
        ));
    }

    fn prepare_chat_completion_persistence(
        &mut self,
        request: &SessionSaveRequest<'_>,
        tool_results: &[ToolResult],
    ) -> Result<session_persistence::PendingExecutionContextPersistence> {
        if self.dry_run {
            return Ok(session_persistence::PendingExecutionContextPersistence::none(""));
        }

        self.prepare_save_message(request, tool_results)
    }

    /// Record an assistant tool-call request BEFORE the tools execute.
    /// Writes a `ToolCalls` entry to the session log and pushes a
    /// pending Tool message in-memory.  Must be paired with a
    /// [`append_session_tool_results`] call once outputs are available.
    ///
    /// No-ops if no session is active; errors if persistence fails.
    pub fn append_session_tool_calls(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        calls: &[crate::tool::ToolCall],
    ) -> Result<()> {
        let request = SessionSaveRequest::new(input, output, thought);
        let Some(session) = self.session_for_save(&request) else {
            return Ok(());
        };
        crate::config::session::add_tool_calls(
            session,
            &request.input,
            request.output,
            request.thought,
            calls,
        )
    }

    /// Finalize the tool round opened by [`append_session_tool_calls`].
    /// Writes a `ToolResults` entry to the session log and fills in
    /// the pending outputs on the last in-memory message.
    pub async fn append_session_tool_results(&mut self, results: &[ToolResult]) -> Result<()> {
        let persistence = self.prepare_session_tool_results(results)?;
        persistence.persist().await;
        Ok(())
    }

    pub(crate) fn prepare_session_tool_results(
        &mut self,
        results: &[ToolResult],
    ) -> Result<session_persistence::PendingExecutionContextPersistence> {
        let Some(session) = self.session.as_mut() else {
            return Ok(session_persistence::PendingExecutionContextPersistence::none(""));
        };
        crate::config::session::prepare_tool_results(session, results)
    }

    pub async fn save_message(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        tool_results: &[crate::tool::ToolResult],
    ) -> Result<()> {
        self.persist_message(
            SessionSaveRequest::new(input, output, thought),
            tool_results,
        )
        .await
    }

    async fn persist_message(
        &mut self,
        request: SessionSaveRequest<'_>,
        tool_results: &[crate::tool::ToolResult],
    ) -> Result<()> {
        await_context_persistence(self.prepare_save_message(&request, tool_results)).await
    }

    fn prepare_save_message(
        &mut self,
        request: &SessionSaveRequest<'_>,
        tool_results: &[crate::tool::ToolResult],
    ) -> Result<session_persistence::PendingExecutionContextPersistence> {
        let Some(session) = self.session_for_save(request) else {
            return Ok(session_persistence::PendingExecutionContextPersistence::none(""));
        };
        if tool_results.is_empty() {
            crate::config::session::add_assistant_text(
                session,
                &request.input,
                request.output,
                request.thought,
            )?;
            return Ok(session_persistence::PendingExecutionContextPersistence::none(session.id()));
        }

        Self::save_message_with_tool_results(session, request, tool_results)
    }

    fn session_for_save<'a>(&'a mut self, request: &SessionSaveRequest) -> Option<&'a mut Session> {
        if !request.input.with_session() {
            return None;
        }

        self.session.as_mut()
    }

    fn save_message_with_tool_results(
        session: &mut Session,
        request: &SessionSaveRequest,
        tool_results: &[crate::tool::ToolResult],
    ) -> Result<session_persistence::PendingExecutionContextPersistence> {
        let calls = collect_tool_calls(tool_results);
        crate::config::session::add_tool_calls(
            session,
            &request.input,
            request.output,
            request.thought,
            &calls,
        )?;
        crate::config::session::prepare_tool_results(session, tool_results)
    }
}

pub(crate) fn collect_tool_calls(
    tool_results: &[crate::tool::ToolResult],
) -> Vec<crate::tool::ToolCall> {
    tool_results
        .iter()
        .map(|result| result.call.clone())
        .collect()
}
