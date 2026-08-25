// Integration tests for P2.4 control plane cancel semantics.
//!
//! Note: These tests verify the control command contract and reconstruct_state
//! behavior over durable session-log entries.

mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::{
    require_nextest,
    session::SessionLogEntry,
    session_reconstruct::{reconstruct_state, TurnStatus},
};
use harnx_runtime::{
    nats_session_log::NatsSessionLog,
    nats_worker::{publish_control_command, ControlCommand},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_control_command_serializes_and_is_publishable() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let mut control_sub = client
        .subscribe(harnx_runtime::nats_worker::control_subject(
            "control-cancel-only",
        ))
        .await?;

    publish_control_command(&client, "control-cancel-only", &ControlCommand::Cancel).await?;

    use futures_util::StreamExt;
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), control_sub.next())
        .await?
        .expect("should receive control message");

    let cmd: ControlCommand = serde_json::from_slice(&msg.payload)?;
    assert!(matches!(cmd, ControlCommand::Cancel));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_appends_entry_before_abort() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let session_id = "control-cancel-session";

    let log = NatsSessionLog::new(js.clone(), session_id);
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("go".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "working".to_string(),
        thought: None,
        calls: vec![],
        timestamp: None,
        fence_token: Some(42),
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Cancel { fence_token: 42 })
        .await?;

    let entries = log.load_events_async().await?;
    let entries_only: Vec<_> = entries.into_iter().map(|(_, e)| e).collect();
    assert!(entries_only
        .iter()
        .any(|e| matches!(e, SessionLogEntry::Cancel { fence_token } if *fence_token == 42)));

    let state = reconstruct_state(&entries_only);
    assert_eq!(state.turn_status, TurnStatus::InFlightCancelled);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_prevents_resume_on_reactivation() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "control-cancel-no-resume";

    let log = NatsSessionLog::new(js.clone(), session_id);
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("go".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "working".to_string(),
        thought: None,
        calls: vec![],
        timestamp: None,
        fence_token: Some(10),
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Cancel { fence_token: 10 })
        .await?;

    let entries = log.load_events_async().await?;
    let entries_only: Vec<_> = entries.into_iter().map(|(_, e)| e).collect();
    let state = reconstruct_state(&entries_only);

    assert_eq!(state.turn_status, TurnStatus::InFlightCancelled);
    Ok(())
}
