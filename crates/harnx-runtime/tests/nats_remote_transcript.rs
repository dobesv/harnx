mod common;

use anyhow::{Context, Result};
use common::spawn_nats_server;
use harnx_core::{
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
    tool::ToolCall,
};
use harnx_runtime::{
    config::remote_session_ops::{load_remote_transcript_for_render, RemoteTranscriptState},
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer,
};
use serde_json::json;

const PENDING_TOOL_RESPONSE_ERROR: &str = "tool response pending (results not yet persisted)";
const LOST_TOOL_RESPONSE_ERROR: &str =
    "tool response lost (session was interrupted before results were persisted)";

fn trailing_tool_error(transcript: &RemoteTranscriptState) -> String {
    let tail = transcript.messages.last().expect("trailing tool message");
    assert_eq!(tail.role, MessageRole::Tool);
    let MessageContent::ToolCalls(tool_calls) = &tail.content else {
        panic!("expected tool-call content on trailing message");
    };
    tool_calls
        .tool_results
        .first()
        .expect("synthetic tool result")
        .output
        .get("error")
        .and_then(serde_json::Value::as_str)
        .expect("synthetic tool error")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_transcript_marks_trailing_tool_call_pending_only_while_leased() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = format!("test-{}", uuid::Uuid::new_v4());
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "test".to_string(),
            initializer: SessionInitializer::named("test-agent", Default::default()),
            session_id: Some(session_id.clone()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        client,
        jetstream.clone(),
        harnx_runtime::utils::create_abort_signal(),
    )
    .await?;
    NatsSessionLog::new(jetstream.clone(), session_id.clone())
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: "running tool".to_string(),
            thought: None,
            calls: vec![ToolCall::new(
                "long_running_tool".to_string(),
                json!({"task": "work"}),
                Some("call-pending".to_string()),
                None,
            )],
            timestamp: None,
            fence_token: Some(1),
        })
        .await?;

    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream,
        session_id: &session_id,
        worker_id: "worker-a".to_string(),
        generation: 1,
        config: NatsLeaseConfig {
            replicas: 1,
            ..Default::default()
        },
        session_metadata: None,
    })
    .await?
    .context("acquire test session lease")?;

    let leased = load_remote_transcript_for_render(&session).await?;
    let leased_error = trailing_tool_error(&leased);
    assert_eq!(leased_error, PENDING_TOOL_RESPONSE_ERROR);
    assert!(!leased_error.contains("tool response lost"));

    lease.release().await?;

    let unleased = load_remote_transcript_for_render(&session).await?;
    let unleased_error = trailing_tool_error(&unleased);
    assert_eq!(unleased_error, LOST_TOOL_RESPONSE_ERROR);
    assert!(!unleased_error.contains("tool response pending"));

    Ok(())
}
