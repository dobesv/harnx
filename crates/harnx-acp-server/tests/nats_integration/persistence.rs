//! Durable transcript loading and identity-scoping behavior.

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    ContentBlock as AcpContentBlock, SessionId, SessionUpdate,
};
use anyhow::{Context, Result};
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::tool::ToolCall;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
use harnx_runtime::{AgentCallFn, SessionInitializer};
use tokio::sync::mpsc;

use super::support::*;

fn mock_response_worker_fn(response: &'static str) -> AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        Box::pin(async move {
            harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(response.to_string())],
            }));
            Ok((
                response.to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_replays_history_and_establishes_session_context() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let log = create_fixture(
        &config,
        FixtureSpec {
            agent: AGENT_NAME,
            session_id: "load-session",
            entries: load_fixture_entries(),
        },
    )
    .await?;
    let agent = new_agent(&config);

    // Spawn worker that responds to prompts on the loaded session
    let worker = spawn_worker(
        Arc::clone(&config),
        mock_response_worker_fn("loaded response"),
    )
    .await?;

    let mut client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;

    // Load the session - this should replay history AND establish session context
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

    // CRITICAL: After load, prompt should succeed (not "session not found")
    let response = tokio::time::timeout(
        TEST_TIMEOUT,
        agent.prompt(text_prompt(
            SessionId::new("load-session".to_string()),
            "continue",
        )),
    )
    .await
    .context("prompt on loaded session timed out")?
    .context("prompt on loaded session failed")?;

    // Verify the session responded
    assert_eq!(
        response.stop_reason,
        agent_client_protocol::schema::v1::StopReason::EndTurn
    );

    // Receive the prompt response chunks
    let mut chunks = Vec::new();
    while chunks.is_empty() {
        let notification = tokio::time::timeout(TEST_TIMEOUT, client.notifications.recv())
            .await
            .context("timed out waiting for session/update on loaded session")?
            .context("ACP notification stream closed")?;
        assert_eq!(notification.session_id.0.as_ref(), "load-session");
        if let Some(text) = notification_text(notification) {
            chunks.push(text);
        }
    }
    assert_eq!(chunks, ["loaded response"]);

    worker.abort();
    let _ = worker.await;
    drop(log);
    Ok(())
}

/// Fixture entries that include a HandoffCommitted record.
/// Used to verify that loaded handed-off sessions reject prompts.
fn handoff_fixture_entries() -> Vec<SessionLogEntry> {
    let mut entries = load_fixture_entries();
    entries.push(SessionLogEntry::HandoffCommitted {
        target_agent: "atlas@prod".to_string(),
        target_session_id: "control-target".to_string(),
        handoff_tool_call_id: Some("control-handoff".to_string()),
    });
    entries
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_handed_off_session_deactivates_and_rejects_prompt() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    // Create a session fixture WITH HandoffCommitted
    let log = create_fixture(
        &config,
        FixtureSpec {
            agent: AGENT_NAME,
            session_id: "handed-off-session",
            entries: handoff_fixture_entries(),
        },
    )
    .await?;
    let agent = new_agent(&config);
    let mut client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;

    // Load the handed-off session
    client
        .connection
        .load_session("handed-off-session".to_string(), std::env::current_dir()?)
        .block_task()
        .start_session()
        .await
        .context("load_session failed")?;

    // Drain replay updates - handoff fixture has same 5 entries as standard fixture
    // HandoffCommitted and other control entries are silent during replay
    for _ in 0..5 {
        let notification = tokio::time::timeout(TEST_TIMEOUT, client.notifications.recv())
            .await
            .context("durable replay update timed out")?
            .context("ACP notification stream closed during replay")?;
        assert_eq!(notification.session_id.0.as_ref(), "handed-off-session");
    }
    // Ensure no extra updates
    assert!(
        matches!(
            client.notifications.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ),
        "load replay emitted duplicate or unexpected updates"
    );

    // CRITICAL: Prompt must fail because the session is handed-off (deactivated)
    let prompt_error = agent
        .prompt(text_prompt(
            SessionId::new("handed-off-session".to_string()),
            "attempted prompt",
        ))
        .await
        .expect_err("prompt on handed-off session must fail");

    let error_message = prompt_error.to_string();

    // Verify the error contains handoff-specific guidance
    for expected in [
        "no longer active",
        "new prompt was not sent to the source session",
        "agent `atlas`",
        "local session `control-target`",
        "cluster `prod`",
    ] {
        assert!(
            error_message.contains(expected),
            "missing `{expected}` in error: {error_message}"
        );
    }

    drop(log);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_rejects_another_agents_session() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    create_fixture(
        &config,
        FixtureSpec {
            agent: "other-agent",
            session_id: "shared-id",
            entries: load_fixture_entries(),
        },
    )
    .await?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_fails_without_establishing_context() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let agent = new_agent(&config);
    let client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;

    // Attempt to load a non-existent session
    let error = client
        .connection
        .load_session("nonexistent-session".to_string(), std::env::current_dir()?)
        .block_task()
        .start_session()
        .await
        .expect_err("load of nonexistent session must fail");
    let message = error.to_string();
    assert!(
        message.contains("nonexistent-session"),
        "unexpected error: {message}"
    );

    // CRITICAL: Prompt must fail with "session not found" (no context was established)
    let prompt_error = agent
        .prompt(text_prompt(
            SessionId::new("nonexistent-session".to_string()),
            "test",
        ))
        .await
        .expect_err("prompt on nonexistent loaded session must fail");
    let prompt_message = prompt_error.to_string();
    assert!(
        prompt_message.contains("session not found"),
        "expected 'session not found' error, got: {prompt_message}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_cancel_works_on_loaded_session() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    // Create a standard session fixture (no handoff)
    let log = create_fixture(
        &config,
        FixtureSpec {
            agent: AGENT_NAME,
            session_id: "cancel-test-session",
            entries: load_fixture_entries(),
        },
    )
    .await?;
    let agent = new_agent(&config);
    let mut client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;

    // Load the session
    client
        .connection
        .load_session("cancel-test-session".to_string(), std::env::current_dir()?)
        .block_task()
        .start_session()
        .await
        .context("load_session failed")?;

    // Drain the replay updates - use a helper that doesn't re-load
    for _ in 0..5 {
        let notification = tokio::time::timeout(TEST_TIMEOUT, client.notifications.recv())
            .await
            .context("durable replay update timed out")?
            .context("ACP notification stream closed during replay")?;
        assert_eq!(notification.session_id.0.as_ref(), "cancel-test-session");
    }
    // Ensure no extra updates
    assert!(
        matches!(
            client.notifications.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ),
        "load replay emitted duplicate or unexpected updates"
    );

    // Cancel should work on a loaded session (not return "session not found")
    agent
        .cancel(agent_client_protocol::schema::v1::CancelNotification::new(
            SessionId::new("cancel-test-session".to_string()),
        ))
        .await
        .context("cancel should succeed on loaded session")?;

    // Verify we can still query the session (it exists)
    assert!(
        agent
            .session_last_touched("cancel-test-session")
            .await
            .is_some(),
        "loaded session should still exist after cancel"
    );

    drop(log);
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

/// Parameters for creating a test session fixture.
struct FixtureSpec<'a> {
    agent: &'a str,
    session_id: &'a str,
    entries: Vec<SessionLogEntry>,
}

async fn create_fixture(config: &GlobalConfig, spec: FixtureSpec<'_>) -> Result<NatsSessionLog> {
    let config = config.read().clone();
    let jetstream = config.nats_jetstream(CLUSTER).await?;
    let replicas = 1;
    let initializer = SessionInitializer::named(spec.agent, Default::default());
    let metadata = SessionMetadata::new(spec.session_id, initializer.clone());
    let store = SessionMetadataStore::ensure(&jetstream, replicas).await?;
    store
        .create(&metadata)
        .await?
        .context("fixture metadata already exists")?;
    let log = NatsSessionLog::new_with_replicas(
        jetstream,
        initializer.session_key(spec.session_id),
        replicas,
    );
    for entry in spec.entries {
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
        // Note: HandoffCommitted and HitlApprovalRequested are removed from the standard fixture.
        // Sessions with handoffs are tested separately in session_load_handed_off_session_deactivates_and_rejects_prompt.
    ]
}
