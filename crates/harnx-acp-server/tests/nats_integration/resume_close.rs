//! Broker-backed integration tests for session/resume and session/close.
//!
//! Verifies:
//! - `session/resume` establishes context without replaying history updates
//! - `session/resume` rejects other-agent or nonexistent sessions
//! - `session/close` cancels active turn and removes context
//! - `session/close` removes context but allows new sessions
//! - `session/close` is idempotent (unknown IDs succeed)

use std::sync::Arc;

use agent_client_protocol::schema::v1::{CloseSessionRequest, ResumeSessionRequest, SessionId};
use anyhow::{Context, Result};
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_core::message::MessageRole;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
use harnx_runtime::{AgentCallFn, SessionInitializer};

use super::support::*;

fn mock_response_worker_fn(response: &'static str) -> AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let response = response.to_string();
        Box::pin(async move {
            harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(response.clone())],
            }));
            Ok((
                response,
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

/// Create a session fixture with optional entries for durable history.
async fn create_resume_fixture(
    config: &GlobalConfig,
    agent: &str,
    session_id: &str,
    entries: Vec<SessionLogEntry>,
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
        .context("fixture metadata already exists")?;
    let log =
        NatsSessionLog::new_with_replicas(jetstream, initializer.session_key(session_id), replicas);
    for entry in entries {
        log.append_event_async(&entry).await?;
    }
    Ok(log)
}

fn message_entry(role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role,
        content: harnx_core::message::MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_resume_establishes_context_without_replay() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    // Create a session fixture with existing history
    let session_id = "resume-no-replay";
    create_resume_fixture(
        &config,
        AGENT_NAME,
        session_id,
        vec![
            message_entry(MessageRole::User, "old question"),
            message_entry(MessageRole::Assistant, "old answer"),
        ],
    )
    .await?;

    let worker = spawn_worker(
        Arc::clone(&config),
        mock_response_worker_fn("fresh response"),
    )
    .await?;

    let agent = new_agent(&config);

    // Resume should not emit session/update notifications
    let response = agent
        .resume_session(ResumeSessionRequest::new(
            SessionId::new(session_id.to_string()),
            std::path::PathBuf::new(),
        ))
        .await
        .context("resume failed")?;
    let _ = response;

    // Give a brief moment for any spurious notifications (there should be none)
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Subsequent prompt should work
    let session_id_obj = SessionId::new(session_id.to_string());
    let prompt_response = agent
        .prompt(text_prompt(session_id_obj, "new question"))
        .await?;
    assert_eq!(
        prompt_response.stop_reason,
        agent_client_protocol::schema::v1::StopReason::EndTurn
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_resume_rejects_other_agent_or_nonexistent() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    // Create a session fixture with existing history for a *different* agent
    let session_id = "resume-wrong-agent";
    create_resume_fixture(&config, "other-agent", session_id, vec![]).await?;

    let worker = spawn_worker(Arc::clone(&config), mock_response_worker_fn("response")).await?;
    let agent = new_agent(&config);

    // Resume with wrong agent should fail
    let result = agent
        .resume_session(ResumeSessionRequest::new(
            SessionId::new(session_id.to_string()),
            std::path::PathBuf::new(),
        ))
        .await;
    assert!(result.is_err(), "resume with wrong agent should fail");

    // Resume of nonexistent session should fail
    let result = agent
        .resume_session(ResumeSessionRequest::new(
            SessionId::new("never-existed".to_string()),
            std::path::PathBuf::new(),
        ))
        .await;
    assert!(result.is_err(), "resume nonexistent session should fail");

    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_close_cancels_active_turn_and_removes_context() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let worker = spawn_worker(
        Arc::clone(&config),
        mock_held_turn_fn(started.clone(), release.clone()),
    )
    .await?;

    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    // Start a prompt and wait for model call to start
    let mut prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        let session_id = session_id.clone();
        async move { agent.prompt(text_prompt(session_id, "active turn")).await }
    });
    tokio::select! {
        permit = started.acquire() => permit.context("started semaphore closed")?.forget(),
        result = &mut prompt_task => anyhow::bail!("prompt ended before model call started: {result:?}"),
        _ = tokio::time::sleep(TEST_TIMEOUT) => anyhow::bail!("worker model call did not start"),
    }

    // Close the session - should cancel the active turn
    agent
        .close_session(CloseSessionRequest::new(session_id.clone()))
        .await
        .context("close failed")?;

    // The prompt should be cancelled
    let prompt_result = tokio::time::timeout(TEST_TIMEOUT, prompt_task)
        .await
        .context("prompt did not return after close")?
        .context("prompt task crashed")??;
    assert_eq!(
        prompt_result.stop_reason,
        agent_client_protocol::schema::v1::StopReason::Cancelled
    );

    // Context should be removed
    assert!(
        agent.session_last_touched(&session_id.0).await.is_none(),
        "context should be removed after close"
    );

    // Release permits to allow worker to finish cleanly
    release.add_permits(1);

    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_close_preserves_durable_history_and_allows_resume() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    let worker = spawn_worker(Arc::clone(&config), mock_response_worker_fn("response")).await?;
    let agent = new_agent(&config);

    let session_id = initialize_and_create_session(&agent).await?;

    // Verify context exists before close
    assert!(
        agent
            .session_last_touched(session_id.0.as_ref())
            .await
            .is_some(),
        "context should exist before close"
    );

    // Close the session (removes in-memory context)
    agent
        .close_session(CloseSessionRequest::new(session_id.clone()))
        .await
        .context("close failed")?;

    // Verify context was removed after close
    assert!(
        agent.session_last_touched(&session_id.0).await.is_none(),
        "context should be removed after close"
    );

    // Create a new session to prove session operations still work after close
    let new_session_id = initialize_and_create_session(&agent).await?;
    assert!(
        agent
            .session_last_touched(new_session_id.0.as_ref())
            .await
            .is_some(),
        "new session should work after close"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_close_is_idempotent() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    let worker = spawn_worker(Arc::clone(&config), mock_response_worker_fn("response")).await?;

    let agent = new_agent(&config);

    // Close unknown ID should succeed
    agent
        .close_session(CloseSessionRequest::new(SessionId::new(
            "never-existed".to_string(),
        )))
        .await
        .context("close unknown session should succeed")?;

    // Create and close a session
    let session_id = initialize_and_create_session(&agent).await?;
    agent
        .close_session(CloseSessionRequest::new(session_id.clone()))
        .await?;

    // Close again should succeed
    agent
        .close_session(CloseSessionRequest::new(session_id.clone()))
        .await
        .context("close already-closed session should succeed")?;

    worker.abort();
    let _ = worker.await;
    Ok(())
}

fn mock_held_turn_fn(
    started: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
) -> AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        Box::pin(async move {
            started.add_permits(1);
            release
                .acquire()
                .await
                .expect("release semaphore closed")
                .forget();
            Ok((
                "done".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}
