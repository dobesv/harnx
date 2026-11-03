//! Journal setup and failure assertions for pre-turn recovery size rejection.
use super::*;
use harnx_toolset::{ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;

async fn append_orphan_round(log: &NatsSessionLog) -> Result<u64> {
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "working".into(),
        thought: None,
        calls: vec![ToolCall::new(
            "query".into(),
            json!({}),
            Some("call".into()),
            None,
        )],
        timestamp: None,
        fence_token: None,
    })
    .await
}

pub(super) async fn seed_journaled_reply(
    js: &async_nats::jetstream::Context,
    log: &NatsSessionLog,
    key: &str,
    session_id: &str,
) -> Result<()> {
    let round = append_orphan_round(log).await?;
    let journal = InvocationJournal::ensure(js, 1).await?;
    let request = ToolRequest {
        call_id: "operation".into(),
        operation_id: "operation".into(),
        tool: "query".into(),
        args: json!({}),
        parent_session_id: Some(key.to_owned()),
        parent_agent: None,
        parent_local_session_id: Some(session_id.into()),
        tool_call_id: Some("call".into()),
        capabilities: Default::default(),
        tool_round: Some(round),
        run_context: None,
        replay: None,
    };
    journal
        .record(&request, ("query", "scope", "server"))
        .await?;
    journal
        .complete(
            &request,
            ToolReply {
                call_id: request.call_id.clone(),
                result: Ok(json!({"data": "x".repeat(8192)})),
                final_progress: None,
            },
        )
        .await?;
    Ok(())
}

pub(super) async fn assert_recovery_rejected(
    log: &NatsSessionLog,
    calls: &AtomicUsize,
) -> Result<()> {
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "failure precedes turn body"
    );
    let entries = log.load_events_async().await?;
    let errors = error_messages(&entries);
    assert_eq!(errors.len(), 1, "first failure must persist its Error");
    assert!(errors[0].contains("JetStream did not ack session log entry"));
    Ok(())
}
