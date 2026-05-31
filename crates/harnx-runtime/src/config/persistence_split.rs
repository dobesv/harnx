//! Chat-completion persistence methods extracted from config/mod.rs for code health.
use super::*;

impl Config {
    pub fn after_chat_completion(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        tool_results: &[ToolResult],
        usage: &crate::client::CompletionTokenUsage,
    ) -> Result<()> {
        self.record_completion_usage(usage);
        self.update_last_message_after_completion(input, output, tool_results);
        self.persist_chat_completion(input, output, thought, tool_results)
    }

    fn update_last_message_after_completion(
        &mut self,
        input: &Input,
        output: &str,
        tool_results: &[ToolResult],
    ) {
        if !tool_results.is_empty() {
            return;
        }

        self.last_message = Some(LastMessage::new(input.clone(), output.to_string()));
    }

    fn persist_chat_completion(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        tool_results: &[ToolResult],
    ) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        self.save_message(input, output, thought, tool_results)
    }

    /// Record an assistant tool-call request BEFORE the tools execute.
    /// Writes a `ToolCalls` entry to the session log and pushes a
    /// pending Tool message in-memory.  Must be paired with a
    /// [`save_session_tool_results`] call once outputs are available.
    ///
    /// Errors if no session is active or persistence fails.
    pub fn save_session_tool_calls(
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

    pub fn save_message(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        tool_results: &[crate::tool::ToolResult],
    ) -> Result<()> {
        let request = SessionSaveRequest::new(input, output, thought);
        let Some(session) = self.session_for_save(&request) else {
            return Ok(());
        };
        if tool_results.is_empty() {
            return crate::config::session::add_assistant_text(
                session,
                &request.input,
                request.output,
                request.thought,
            );
        }

        Self::save_message_with_tool_results(session, &request, tool_results)
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
