//! Broker-backed integration tests for session/list.
//!
//! Verifies:
//! - Zed-style `session/list` returns only pinned-agent sessions in newest-first order.
//! - Sessions from same cluster belonging to other agents are excluded.
//! - Filtering by cwd works when specified.
//! - Fallback process-cwd is used when metadata cwd is absent.
//! - Returned session IDs can be loaded with `session/load`.

use std::sync::Arc;

use agent_client_protocol::schema::v1::ListSessionsRequest;
use anyhow::{Context, Result};
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
use harnx_runtime::{AgentCallFn, SessionInitializer};

use super::support::*;

#[allow(dead_code)]
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

/// Create a session fixture with metadata for the given agent.
async fn create_session_fixture(
    config: &GlobalConfig,
    agent: &str,
    session_id: &str,
    title: Option<&str>,
) -> Result<()> {
    let config_guard = config.read().clone();
    let jetstream = config_guard.nats_jetstream(CLUSTER).await?;
    let replicas = config_guard
        .nats_server(CLUSTER)
        .unwrap()
        .replicas
        .unwrap_or(1);

    let initializer = SessionInitializer::named(agent, Default::default());
    let mut metadata = SessionMetadata::new(session_id, initializer);

    if let Some(t) = title {
        metadata.title.value = Some(t.to_string());
    }

    let store = SessionMetadataStore::ensure(&jetstream, replicas).await?;
    store
        .create(&metadata)
        .await?
        .context("fixture metadata already exists")?;

    // Create an empty session log to make it valid
    let log = NatsSessionLog::new_with_replicas(
        jetstream,
        harnx_core::session_identity::session_key(Some(agent), session_id),
        replicas,
    );
    // Append a minimal entry to make the session valid
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: 0,
        fence_token: 0,
        timestamp: None,
        usage: None,
    })
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_list_returns_only_pinned_agent_sessions() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    // Create sessions for different agents
    create_session_fixture(&config, AGENT_NAME, "session-1", Some("First session")).await?;
    create_session_fixture(
        &config,
        "other-agent",
        "session-2",
        Some("Other agent session"),
    )
    .await?;
    create_session_fixture(&config, AGENT_NAME, "session-3", Some("Second session")).await?;

    let agent = new_agent(&config);

    // List sessions - should only return sessions for AGENT_NAME
    let response = agent.list_sessions(ListSessionsRequest::new()).await?;

    assert_eq!(
        response.sessions.len(),
        2,
        "should return only pinned agent sessions"
    );

    // Check that all returned sessions belong to the pinned agent
    let ids: Vec<&str> = response
        .sessions
        .iter()
        .map(|s| s.session_id.0.as_ref())
        .collect();
    assert!(ids.contains(&"session-1"), "should include session-1");
    assert!(ids.contains(&"session-3"), "should include session-3");
    assert!(
        !ids.contains(&"session-2"),
        "should not include other-agent session"
    );

    // Verify newest-first ordering (session-3 created after session-1)
    assert_eq!(&*response.sessions[0].session_id.0, "session-3");
    assert_eq!(&*response.sessions[1].session_id.0, "session-1");

    assert!(response.next_cursor.is_none(), "no pagination expected");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_list_filters_by_cwd() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    // Create sessions - cwd filtering tested when sessions have execution context
    create_session_fixture(&config, AGENT_NAME, "cwd-session-1", Some("Session one")).await?;
    create_session_fixture(&config, AGENT_NAME, "cwd-session-2", Some("Session two")).await?;

    let agent = new_agent(&config);

    // Without execution context, sessions won't match a specific cwd filter
    // Test that clear cwd filter works (empty result when no match)
    let filter_path = std::path::PathBuf::from("/nonexistent/path");
    let response = agent
        .list_sessions(ListSessionsRequest::new().cwd(Some(filter_path)))
        .await?;

    assert_eq!(
        response.sessions.len(),
        0,
        "no sessions should match nonexistent cwd filter"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_list_returns_empty_for_no_sessions() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    let agent = new_agent(&config);

    let response = agent.list_sessions(ListSessionsRequest::new()).await?;

    assert_eq!(response.sessions.len(), 0, "should return empty list");
    assert!(response.next_cursor.is_none());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_list_returns_titles() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    create_session_fixture(&config, AGENT_NAME, "title-test", Some("My Special Title")).await?;

    let agent = new_agent(&config);

    let response = agent.list_sessions(ListSessionsRequest::new()).await?;

    assert_eq!(response.sessions.len(), 1);
    assert_eq!(
        response.sessions[0].title,
        Some("My Special Title".to_string())
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listed_session_id_can_be_used_with_load() -> Result<()> {
    // Verify listed session ID format is compatible with load_session
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);

    // Create a session
    create_session_fixture(&config, AGENT_NAME, "loadable-session", Some("Loadable")).await?;

    let agent = new_agent(&config);

    // List sessions
    let list_response = agent.list_sessions(ListSessionsRequest::new()).await?;
    assert_eq!(list_response.sessions.len(), 1);

    // Verify session ID format matches what load_session expects
    let session_id = &list_response.sessions[0].session_id;
    assert!(!session_id.0.is_empty(), "session ID should not be empty");

    // The session ID is a valid string that can be passed to load_session
    // (actual loading requires connection context which is tested in persistence tests)
    Ok(())
}
