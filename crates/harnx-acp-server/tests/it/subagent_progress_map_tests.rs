use agent_client_protocol::schema::v1::{SessionUpdate, ToolCallStatus};
use harnx_acp_server::event_map::agent_event_to_session_update;
use harnx_acp_server::HARNX_USAGE_META;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{AgentEvent, SubAgentProgress, SubAgentProgressStatus, TurnEvent};

#[test]
fn subagent_progress_maps_to_tool_call_update() {
    let usage = CompletionTokenUsage {
        input_tokens: 100,
        output_tokens: 50,
        cached_tokens: 10,
        cache_write_tokens: 5,
    };
    let progress = SubAgentProgress {
        invocation_id: "inv-123".to_string(),
        agent: "atlas".to_string(),
        session_id: "child-session-1".to_string(),
        status: SubAgentProgressStatus::Running,
        elapsed_ms: 15000,
        usage: usage.clone(),
        tool_call_count: 5,
        title: Some("Analyzing codebase".to_string()),
        tool_call_id: Some("parent-call-123".to_string()),
    };

    let update =
        agent_event_to_session_update(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)))
            .expect("subagent progress should map");

    let SessionUpdate::ToolCallUpdate(update) = update else {
        panic!("expected tool call update");
    };

    // Parent transcript tool call id wins over the invocation id.
    assert_eq!(update.tool_call_id.0.as_ref(), "parent-call-123");

    // Title incorporates agent, child title, and compact usage
    let title = update.fields.title.as_ref().expect("title should be set");
    assert!(title.contains("atlas"), "title should contain agent name");
    assert!(
        title.contains("Analyzing codebase"),
        "title should contain child session title"
    );
    assert!(
        title.contains("(100→50)"),
        "title should contain compact usage"
    );

    // Status is InProgress while Running
    assert_eq!(update.fields.status, Some(ToolCallStatus::InProgress));

    // Usage in namespaced _meta
    let meta = update.meta.as_ref().expect("meta should be set");
    let usage_value = meta
        .get(HARNX_USAGE_META)
        .expect("harnx:usage should be in meta");
    let deserialized_usage: CompletionTokenUsage =
        serde_json::from_value(usage_value.clone()).expect("usage should deserialize");
    assert_eq!(deserialized_usage.input_tokens, 100);
    assert_eq!(deserialized_usage.output_tokens, 50);
}

#[test]
fn subagent_progress_falls_back_to_invocation_id_without_parent_tool_call_id() {
    let progress = child_progress("legacy-invocation", SubAgentProgressStatus::Running);
    let update =
        agent_event_to_session_update(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)))
            .expect("subagent progress should map");
    let SessionUpdate::ToolCallUpdate(update) = update else {
        panic!("expected tool call update");
    };
    assert_eq!(update.tool_call_id.0.as_ref(), "legacy-invocation");
}

/// A snapshot with no usage, title or parent call, for tests that only vary
/// the invocation and its status.
fn child_progress(invocation_id: &str, status: SubAgentProgressStatus) -> SubAgentProgress {
    SubAgentProgress {
        invocation_id: invocation_id.to_string(),
        agent: "test".to_string(),
        session_id: "session-1".to_string(),
        status,
        elapsed_ms: 1000,
        usage: CompletionTokenUsage::default(),
        tool_call_count: 0,
        title: None,
        tool_call_id: None,
    }
}

#[test]
fn subagent_progress_preserves_internal_states() {
    // Cancelling and Unconfirmed map to InProgress (ACP can't encode them)
    for status in [
        SubAgentProgressStatus::Cancelling,
        SubAgentProgressStatus::Unconfirmed,
    ] {
        let progress = child_progress("inv-internal", status);
        let update =
            agent_event_to_session_update(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)))
                .expect("should map");
        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected tool call update");
        };
        assert_eq!(
            update.fields.status,
            Some(ToolCallStatus::InProgress),
            "{status:?} should map to InProgress"
        );
    }

    // Cancelled and Failed map to Failed
    for status in [
        SubAgentProgressStatus::Cancelled,
        SubAgentProgressStatus::Failed,
    ] {
        let progress = child_progress("inv-failed", status);
        let update =
            agent_event_to_session_update(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)))
                .expect("should map");
        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected tool call update");
        };
        assert_eq!(
            update.fields.status,
            Some(ToolCallStatus::Failed),
            "{status:?} should map to Failed"
        );
        // Verify zero-usage title omits usage parenthesis
        let title = update.fields.title.expect("title should be present");
        assert_eq!(title, "@ test");
        assert!(!title.contains('('));
        assert!(!title.contains('→'));
    }
}

#[test]
fn subagent_progress_done_keeps_parent_tool_in_progress() {
    // When child session is Done, the parent tool call remains InProgress
    // until the parent tool finishes (don't equate child Done with parent completion).
    let usage = CompletionTokenUsage {
        input_tokens: 200,
        output_tokens: 100,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    let progress = SubAgentProgress {
        invocation_id: "inv-done".to_string(),
        agent: "pytheas".to_string(),
        session_id: "child-session-done".to_string(),
        status: SubAgentProgressStatus::Done,
        elapsed_ms: 30000,
        usage,
        tool_call_count: 10,
        title: Some("Task completed".to_string()),
        tool_call_id: None,
    };

    let update =
        agent_event_to_session_update(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)))
            .expect("should map");
    let SessionUpdate::ToolCallUpdate(update) = update else {
        panic!("expected tool call update");
    };

    assert_eq!(update.fields.status, Some(ToolCallStatus::InProgress));
    assert!(update.fields.title.unwrap().contains("Task completed"));
    assert_eq!(update.fields.content, None);
}
