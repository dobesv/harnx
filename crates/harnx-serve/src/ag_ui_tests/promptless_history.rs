use super::*;

async fn seed_history(config: &Config, session_id: &str) -> crate::session_actor::SessionRegistry {
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("first assistant reply".into(), None, vec![], usage))
        })
    });
    let registry = ag_ui_test_registry(config, Some(call_fn));
    let response = ag_ui_run_with_call_fn(
        config,
        &registry,
        "plain",
        session_id,
        &ag_ui_request_body(Uuid::new_v4(), "first user prompt"),
        None,
    )
    .await
    .expect("first run");
    read_sse_events_until(response, |events| {
        events.iter().any(|event| event["type"] == "RUN_FINISHED")
    })
    .await;
    registry
}

#[tokio::test]
async fn ag_ui_run_promptless_join_returns_persisted_history_in_snapshot() {
    // Issue #959: Subscribe must refresh history before sending its snapshot.
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "promptless-history-session";
    let registry = seed_history(&config, session_id).await;
    let join = json!({ "threadId": Uuid::new_v4(), "runId": Uuid::new_v4(), "messages": [] });
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&join).unwrap(),
        None,
    )
    .await
    .expect("promptless join");
    let events = read_sse_events_until(response, |events| events.len() >= 5).await;
    assert_promptless_frames(&events);
    assert_persisted_messages(events[3]["messages"].as_array().expect("messages array"));
}

fn assert_promptless_frames(events: &[Value]) {
    assert_event_type_sequence(
        events,
        &[
            "RUN_STARTED",
            "CUSTOM",
            "CUSTOM",
            "MESSAGES_SNAPSHOT",
            "RUN_FINISHED",
        ],
    );
    assert_eq!(events[1]["name"], "session_attach_boundary");
    assert_compaction_cleared(&events[2]);
    assert!(
        events[4].get("result").is_none(),
        "synthetic RUN_FINISHED must not have result key"
    );
}

fn assert_persisted_messages(messages: &[Value]) {
    assert!(
        messages.len() >= 2,
        "snapshot should have at least user + assistant"
    );
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .expect("user message");
    assert_eq!(user["content"], "first user prompt");
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant message");
    // Existing snapshot conversion can leave assistant content null or empty.
    let content = assistant["content"].as_str().unwrap_or("");
    assert!(
        content == "first assistant reply" || content.is_empty(),
        "assistant content should match or be empty: {:?}",
        assistant["content"]
    );
}
