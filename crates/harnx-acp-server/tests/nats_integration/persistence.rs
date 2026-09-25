//! Durable transcript loading and identity-scoping behavior.

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    ContentBlock as AcpContentBlock, SessionId, SessionUpdate,
};
use anyhow::{Context, Result};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::tool::ToolCall;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
use harnx_runtime::SessionInitializer;
use tokio::sync::mpsc;

use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_replays_ordered_read_only_snapshot() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let log = create_load_fixture(&config, AGENT_NAME, "load-session").await?;
    let agent = new_agent(&config);
    let mut client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;

    let first = load_updates(&mut client, "load-session", 5).await?;
    let first: Vec<_> = first.into_iter().map(replay_summary).collect();
    assert_eq!(
        first,
        [
            "user:load question",
            "tool-start:load-call:fs_read",
            "agent:checking ",
            "tool-end:load-call:read result",
            "agent:load answer",
        ]
    );
    let error = agent
        .prompt(text_prompt(
            SessionId::new("load-session"),
            "must stay read-only",
        ))
        .await
        .expect_err("loaded snapshot must not become an active prompt session");
    assert!(error
        .to_string()
        .contains("session not found: load-session"));

    log.append_event_async(&message_entry(MessageRole::Assistant, "post-load answer"))
        .await?;
    let second = load_updates(&mut client, "load-session", 6).await?;
    let second: Vec<_> = second.into_iter().map(replay_summary).collect();
    assert_eq!(second[..5], first);
    assert_eq!(second[5], "agent:post-load answer");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_rejects_same_local_id_owned_by_another_agent() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    create_load_fixture(&config, "other-agent", "shared-id").await?;
    let agent = new_agent(&config);
    let client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;

    let error = client
        .connection
        .load_session("shared-id", std::env::current_dir()?)
        .block_task()
        .start_session()
        .await
        .expect_err("bare ID must not load another agent's session");
    let message = error.to_string();
    assert!(message.contains("shared-id"), "unexpected error: {message}");
    assert!(message.contains(AGENT_NAME), "unexpected error: {message}");
    let config_snapshot = config.read().clone();
    assert!(
        harnx_runtime::config::session_metadata_for_agent(
            &config_snapshot,
            &format!("{AGENT_NAME}@{CLUSTER}"),
            "shared-id"
        )
        .await
        .is_err(),
        "load must not create missing metadata under configured identity"
    );
    Ok(())
}
async fn load_updates(
    client: &mut TestClient,
    session_id: &str,
    count: usize,
) -> Result<Vec<SessionUpdate>> {
    let restored = client
        .connection
        .load_session(session_id.to_string(), std::env::current_dir()?)
        .block_task()
        .start_session()
        .await?;
    assert_eq!(
        restored.response(),
        &acp::schema::v1::LoadSessionResponse::new()
    );
    drop(restored);

    let mut updates = Vec::with_capacity(count);
    for index in 0..count {
        let notification = tokio::time::timeout(TEST_TIMEOUT, client.notifications.recv())
            .await
            .with_context(|| format!("durable replay update {index} timed out"))?
            .context("ACP notification stream closed during replay")?;
        assert_eq!(notification.session_id.0.as_ref(), session_id);
        updates.push(notification.update);
    }
    assert!(
        matches!(
            client.notifications.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ),
        "load replay emitted duplicate or unexpected updates"
    );
    Ok(updates)
}

fn replay_summary(update: SessionUpdate) -> String {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => {
            format!("user:{}", chunk_text(chunk.content))
        }
        SessionUpdate::AgentMessageChunk(chunk) => {
            format!("agent:{}", chunk_text(chunk.content))
        }
        SessionUpdate::ToolCall(call) => format!(
            "tool-start:{}:{}",
            call.tool_call_id.0,
            call.name.unwrap_or_default()
        ),
        SessionUpdate::ToolCallUpdate(update) => {
            let text = update
                .fields
                .content
                .and_then(|content| content.into_iter().next())
                .and_then(|content| match content {
                    acp::schema::v1::ToolCallContent::Content(content) => {
                        Some(chunk_text(content.content))
                    }
                    _ => None,
                })
                .unwrap_or_default();
            format!("tool-end:{}:{text}", update.tool_call_id.0)
        }
        other => format!("unexpected:{other:?}"),
    }
}

fn chunk_text(content: AcpContentBlock) -> String {
    match content {
        AcpContentBlock::Text(text) => text.text,
        _ => "<non-text>".to_string(),
    }
}
fn message_entry(role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role,
        content: MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

async fn create_load_fixture(
    config: &GlobalConfig,
    agent: &str,
    session_id: &str,
) -> Result<NatsSessionLog> {
    let config = config.read().clone();
    let jetstream = config.nats_jetstream(CLUSTER).await?;
    let replicas = 1;
    let initializer = SessionInitializer::named(agent, Default::default());
    let metadata = SessionMetadata::new(session_id, initializer.clone());
    let store = SessionMetadataStore::ensure(&jetstream, replicas).await?;
    store
        .create(&metadata)
        .await?
        .context("load fixture metadata already exists")?;
    let log =
        NatsSessionLog::new_with_replicas(jetstream, initializer.session_key(session_id), replicas);
    for entry in load_fixture_entries() {
        log.append_event_async(&entry).await?;
    }
    Ok(log)
}

fn load_fixture_entries() -> Vec<SessionLogEntry> {
    let call = ToolCall::new(
        "fs_read".to_string(),
        serde_json::json!({"path": "/tmp/replay"}),
        Some("load-call".to_string()),
        None,
    );
    vec![
        message_entry(MessageRole::User, "load question"),
        SessionLogEntry::ToolCalls {
            text: "checking ".to_string(),
            thought: None,
            calls: vec![call],
            timestamp: None,
            fence_token: None,
        },
        SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("load-call".to_string()),
                name: "fs_read".to_string(),
                output: serde_json::json!("file contents"),
                markdown: Some("read result".to_string()),
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        },
        message_entry(MessageRole::Assistant, "load answer"),
        SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 1,
            timestamp: None,
            usage: None,
        },
        SessionLogEntry::HandoffCommitted {
            target_agent: "atlas@prod".to_string(),
            target_session_id: "control-target".to_string(),
            handoff_tool_call_id: Some("control-handoff".to_string()),
        },
        SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "control-approval".to_string(),
            summary: "control record must stay silent".to_string(),
            fence_token: 1,
        },
    ]
}
