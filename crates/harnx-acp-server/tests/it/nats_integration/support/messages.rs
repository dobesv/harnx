//! ACP prompt and notification helpers shared across behavior suites.

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    ContentBlock as AcpContentBlock, PromptRequest, PromptResponse, SessionId, SessionNotification,
    SessionUpdate, TextContent,
};

pub(crate) fn text_prompt(session_id: SessionId, text: &str) -> PromptRequest {
    PromptRequest::new(
        session_id,
        vec![AcpContentBlock::Text(TextContent::new(text.to_string()))],
    )
}

pub(crate) fn notification_text(notification: SessionNotification) -> Option<String> {
    match notification.update {
        SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            AcpContentBlock::Text(text) => Some(text.text),
            _ => None,
        },
        _ => None,
    }
}
pub(crate) fn spawn_prompt(
    agent: Arc<harnx_acp_server::HarnxAgent>,
    session_id: SessionId,
) -> tokio::task::JoinHandle<acp::Result<PromptResponse>> {
    tokio::spawn(async move {
        agent
            .prompt(text_prompt(session_id, "touch lifecycle"))
            .await
    })
}
