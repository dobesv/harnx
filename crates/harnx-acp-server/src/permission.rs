//! ACP bridge for harnx's per-turn tool confirmation callback.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use harnx_runtime::nats_tool_confirmation::{ToolConfirmationHandler, ToolConfirmationRequest};

use crate::AcpConnection;

pub const ALLOW_OPTION_ID: &str = "allow";
pub const REJECT_OPTION_ID: &str = "reject";

pub(crate) fn tool_confirmation_handler(
    connection: Option<AcpConnection>,
    acp_session_id: String,
) -> Arc<ToolConfirmationHandler> {
    Arc::new(move |request| {
        let connection = connection.clone();
        let acp_session_id = acp_session_id.clone();
        Box::pin(
            async move { request_permission(connection.as_ref(), acp_session_id, request).await },
        )
    })
}

async fn request_permission(
    connection: Option<&AcpConnection>,
    acp_session_id: String,
    request: ToolConfirmationRequest,
) -> bool {
    let Some(connection) = connection else {
        tracing::warn!("tool confirmation denied because ACP client is unavailable");
        return false;
    };
    let request = acp_permission_request(acp_session_id, request);
    match connection.send_request(request).block_task().await {
        Ok(response) => outcome_approved(response.outcome),
        Err(error) => {
            tracing::warn!(%error, "ACP permission request failed; denying tool call");
            false
        }
    }
}

fn acp_permission_request(
    acp_session_id: String,
    request: ToolConfirmationRequest,
) -> RequestPermissionRequest {
    let tool_name = request.tool_name;
    let tool_call_id = request
        .tool_call_id
        .filter(|tool_call_id| !tool_call_id.is_empty())
        .unwrap_or_else(|| tool_name.clone());
    let title = request
        .reason
        .filter(|reason| !reason.is_empty())
        .unwrap_or_else(|| tool_name.clone());
    let fields = ToolCallUpdateFields::new()
        .title(title)
        .name(tool_name)
        .status(ToolCallStatus::Pending)
        .raw_input(request.arguments);
    RequestPermissionRequest::new(
        SessionId::new(acp_session_id),
        ToolCallUpdate::new(tool_call_id, fields),
        permission_options(),
    )
}

fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(ALLOW_OPTION_ID, "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new(REJECT_OPTION_ID, "Reject", PermissionOptionKind::RejectOnce),
    ]
}

fn outcome_approved(outcome: RequestPermissionOutcome) -> bool {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => {
            selected.option_id.0.as_ref() == ALLOW_OPTION_ID
        }
        RequestPermissionOutcome::Cancelled => false,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOptionId, SelectedPermissionOutcome};
    use tokio::sync::oneshot;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    fn confirmation_request() -> ToolConfirmationRequest {
        ToolConfirmationRequest {
            session_id: "backend-session".to_string(),
            tool_call_id: Some("call-1".to_string()),
            tool_name: "fs_write".to_string(),
            arguments: serde_json::json!({"path": "/tmp/file"}),
            reason: Some("Write the file?".to_string()),
        }
    }

    #[test]
    fn request_contains_only_single_turn_choices() {
        let request = acp_permission_request("acp-session".to_string(), confirmation_request());

        assert_eq!(request.session_id.0.as_ref(), "acp-session");
        assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "call-1");
        assert_eq!(
            request.tool_call.fields.title.as_deref(),
            Some("Write the file?")
        );
        assert_eq!(request.tool_call.fields.name.as_deref(), Some("fs_write"));
        assert_eq!(
            request.tool_call.fields.raw_input,
            Some(serde_json::json!({"path": "/tmp/file"}))
        );
        assert_eq!(request.options.len(), 2);
        assert_eq!(request.options[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(request.options[1].kind, PermissionOptionKind::RejectOnce);
    }

    #[test]
    fn only_allow_option_approves() {
        let selected = |id| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                PermissionOptionId::new(id),
            ))
        };

        assert!(outcome_approved(selected(ALLOW_OPTION_ID)));
        assert!(!outcome_approved(selected(REJECT_OPTION_ID)));
        assert!(!outcome_approved(selected("unknown")));
        assert!(!outcome_approved(RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn empty_tool_call_id_falls_back_to_tool_name() {
        let mut request = confirmation_request();
        request.tool_call_id = Some(String::new());

        let request = acp_permission_request("acp-session".to_string(), request);

        assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "fs_write");
    }

    #[tokio::test]
    async fn missing_connection_denies() {
        assert!(!request_permission(None, "acp-session".to_string(), confirmation_request()).await);
    }

    #[tokio::test]
    async fn transport_error_denies_permission() {
        let (agent_stream, client_stream) = tokio::io::duplex(4096);
        let (agent_read, agent_write) = tokio::io::split(agent_stream);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let agent_transport = agent_client_protocol::ByteStreams::new(
            agent_write.compat_write(),
            agent_read.compat(),
        );
        let client_transport = agent_client_protocol::ByteStreams::new(
            client_write.compat_write(),
            client_read.compat(),
        );

        let (connection_tx, connection_rx) = oneshot::channel();
        let agent_task = tokio::spawn(async move {
            let _ = agent_client_protocol::Agent
                .builder()
                .connect_with(agent_transport, async move |connection| {
                    let _ = connection_tx.send(connection);
                    std::future::pending::<()>().await;
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .await;
        });
        let client_task = tokio::spawn(async move {
            let _ = agent_client_protocol::Client
                .builder()
                .connect_with(client_transport, async move |_connection| {
                    std::future::pending::<()>().await;
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .await;
        });

        let connection = tokio::time::timeout(std::time::Duration::from_secs(1), connection_rx)
            .await
            .expect("ACP agent connection setup timed out")
            .expect("ACP agent connection closed during setup");
        client_task.abort();

        let denied = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            request_permission(
                Some(&connection),
                "acp-session".to_string(),
                confirmation_request(),
            ),
        )
        .await
        .expect("permission request did not observe transport failure");

        agent_task.abort();
        assert!(!denied);
    }
}
