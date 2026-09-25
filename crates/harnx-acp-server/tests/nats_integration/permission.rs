//! Tool-permission allow, reject, and cancellation behavior.

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    CancelNotification, PermissionOptionKind, RequestPermissionRequest, SessionId, StopReason,
};
use anyhow::{Context, Result};
use harnx_acp_server::permission::{ALLOW_OPTION_ID, REJECT_OPTION_ID};
use harnx_core::tool::ToolCall;
use harnx_runtime::AgentCallFn;
use tokio::sync::mpsc;

use super::support::*;

struct DecisionObserver {
    tx: Option<mpsc::UnboundedSender<bool>>,
}

impl DecisionObserver {
    fn report(mut self, approved: bool) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(approved);
        }
    }
}

impl Drop for DecisionObserver {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(false);
        }
    }
}

fn permission_call_fn(decisions: mpsc::UnboundedSender<bool>) -> AgentCallFn {
    Arc::new(move |_input, config, _abort| {
        let confirm = config
            .read()
            .tui_confirm_tool_use
            .clone()
            .expect("worker should install tool confirmation callback");
        let decisions = decisions.clone();
        Box::pin(async move {
            let observer = DecisionObserver {
                tx: Some(decisions),
            };
            let call = ToolCall::new(
                "fs_write".to_string(),
                serde_json::json!({"path": "/tmp/approved"}),
                Some("call-permission".to_string()),
                None,
            );
            let decision = tokio::task::block_in_place(|| {
                confirm(&call, &call.arguments, Some("Write the test file?"))
            });
            let approved = matches!(decision, harnx_runtime::tool::ToolUseConfirmation::Approve);
            observer.report(approved);
            Ok((
                "permission resolved".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}
fn assert_permission_request(request: &RequestPermissionRequest, session_id: &SessionId) {
    assert_eq!(&request.session_id, session_id);
    assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "call-permission");
    assert_eq!(request.tool_call.fields.name.as_deref(), Some("fs_write"));
    assert_eq!(
        request.tool_call.fields.title.as_deref(),
        Some("Write the test file?")
    );
    assert_eq!(
        request.tool_call.fields.status,
        Some(acp::schema::v1::ToolCallStatus::Pending)
    );
    assert_eq!(
        request.tool_call.fields.raw_input,
        Some(serde_json::json!({"path": "/tmp/approved"}))
    );
    assert_eq!(request.options.len(), 2);
    assert_eq!(request.options[0].option_id.0.as_ref(), ALLOW_OPTION_ID);
    assert_eq!(request.options[0].name, "Allow");
    assert_eq!(request.options[0].kind, PermissionOptionKind::AllowOnce);
    assert_eq!(request.options[1].option_id.0.as_ref(), REJECT_OPTION_ID);
    assert_eq!(request.options[1].name, "Reject");
    assert_eq!(request.options[1].kind, PermissionOptionKind::RejectOnce);
}

async fn permission_round_trip(reply: PermissionReply) -> Result<Option<bool>> {
    let Some(server) = spawn_nats_server().await? else {
        return Ok(None);
    };
    let config = test_config(&server.url);
    let (decision_tx, mut decision_rx) = mpsc::unbounded_channel();
    let worker = spawn_worker(Arc::clone(&config), permission_call_fn(decision_tx)).await?;
    let agent = new_agent(&config);
    let mut client = attach_test_client(Arc::clone(&agent), reply).await?;
    let session_id = initialize_and_create_session(&agent).await?;

    let response = tokio::time::timeout(
        TEST_TIMEOUT,
        agent.prompt(text_prompt(session_id.clone(), "request permission")),
    )
    .await
    .context("permission prompt did not finish")??;
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let request = tokio::time::timeout(TEST_TIMEOUT, client.permissions.recv())
        .await
        .context("ACP permission request timed out")?
        .context("ACP client did not receive permission request")?;
    assert_permission_request(&request, &session_id);
    let approved = tokio::time::timeout(TEST_TIMEOUT, decision_rx.recv())
        .await
        .context("worker permission decision timed out")?
        .context("worker did not receive permission decision")?;

    worker.abort();
    let _ = worker.await;
    Ok(Some(approved))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_once_approves_worker_tool_confirmation() -> Result<()> {
    harnx_core::require_nextest();
    let Some(approved) = permission_round_trip(PermissionReply::Allow).await? else {
        return Ok(());
    };
    assert!(approved);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reject_once_denies_worker_tool_confirmation() -> Result<()> {
    harnx_core::require_nextest();
    let Some(approved) = permission_round_trip(PermissionReply::Reject).await? else {
        return Ok(());
    };
    assert!(!approved);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_permission_denies_and_cancels_turn() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let (decision_tx, mut decision_rx) = mpsc::unbounded_channel();
    let worker = spawn_worker(Arc::clone(&config), permission_call_fn(decision_tx)).await?;
    let agent = new_agent(&config);
    let mut client = attach_test_client(Arc::clone(&agent), PermissionReply::Pending).await?;
    let session_id = initialize_and_create_session(&agent).await?;
    let mut prompt = tokio::spawn({
        let agent = Arc::clone(&agent);
        let session_id = session_id.clone();
        async move {
            agent
                .prompt(text_prompt(session_id, "request permission"))
                .await
        }
    });

    let request = tokio::select! {
        request = client.permissions.recv() => request.context("permission request stream closed")?,
        result = &mut prompt => anyhow::bail!("prompt ended before permission request: {result:?}"),
        _ = tokio::time::sleep(TEST_TIMEOUT) => anyhow::bail!("permission request timed out"),
    };
    assert_permission_request(&request, &session_id);
    agent
        .cancel(CancelNotification::new(session_id))
        .await
        .context("cancel permission prompt")?;
    let response = tokio::time::timeout(TEST_TIMEOUT, prompt)
        .await
        .context("cancelled permission prompt did not return")???;
    assert_eq!(response.stop_reason, StopReason::Cancelled);
    let approved = tokio::time::timeout(TEST_TIMEOUT, decision_rx.recv())
        .await
        .context("worker did not resolve cancelled confirmation")?
        .context("worker confirmation channel closed")?;
    assert!(!approved);

    worker.abort();
    let _ = worker.await;
    Ok(())
}
