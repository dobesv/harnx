use agent_client_protocol::schema::v1::{
    ContentBlock as AcpContentBlock, SessionUpdate, ToolCallContent, ToolCallStatus,
    ToolCallUpdate, ToolKind as AcpToolKind,
};
use harnx_acp_server::event_map::{
    agent_event_to_replay_update, agent_event_to_session_update,
    agent_event_to_session_update_for_cluster, map_tool_status,
};
use harnx_acp_server::{HARNX_ERROR_META, HARNX_MARKDOWN_META};
use harnx_core::event::{
    AgentEvent, AgentSource, ContentBlock, ModelEvent, NoticeEvent, SessionEvent, ToolEvent,
    ToolKind, ToolStatus, TurnEvent, UserEvent,
};

fn text_content(update: &ToolCallUpdate) -> Option<&str> {
    let content = update.fields.content.as_ref()?.first()?;
    let ToolCallContent::Content(content) = content else {
        return None;
    };
    let AcpContentBlock::Text(text) = &content.content else {
        return None;
    };
    Some(&text.text)
}

fn agent_message_text(update: &SessionUpdate) -> Option<&str> {
    let SessionUpdate::AgentMessageChunk(chunk) = update else {
        return None;
    };
    let AcpContentBlock::Text(text) = &chunk.content else {
        return None;
    };
    Some(&text.text)
}

fn replay_message(update: &SessionUpdate) -> (&str, bool) {
    let (chunk, user) = match update {
        SessionUpdate::UserMessageChunk(chunk) => (chunk, true),
        SessionUpdate::AgentMessageChunk(chunk) => (chunk, false),
        _ => panic!("expected replay text chunk"),
    };
    let AcpContentBlock::Text(text) = &chunk.content else {
        panic!("expected replay text content");
    };
    (&text.text, user)
}

struct ExpectedToolUpdate<'a> {
    id: &'a str,
    status: ToolCallStatus,
    output: Option<serde_json::Value>,
    text: Option<&'a str>,
}

fn assert_tool_update(update: &ToolCallUpdate, expected: ExpectedToolUpdate<'_>) {
    assert_eq!(
        (update.tool_call_id.0.as_ref(), update.fields.status),
        (expected.id, Some(expected.status)),
    );
    assert_eq!(
        (update.fields.raw_output.as_ref(), text_content(update)),
        (expected.output.as_ref(), expected.text),
    );
}

#[test]
fn tool_started_maps_to_tool_call() {
    let update = agent_event_to_session_update(AgentEvent::Tool(ToolEvent::Started {
        id: "call-1".to_string(),
        name: "fs_read".to_string(),
        kind: ToolKind::Read,
        markdown: Some("Reading config".to_string()),
        input: serde_json::json!({"path": "/tmp/config"}),
        locations: vec![],
    }))
    .expect("tool start should map");

    let SessionUpdate::ToolCall(call) = update else {
        panic!("expected tool call");
    };
    assert_eq!(
        (call.tool_call_id.0.as_ref(), call.name.as_deref()),
        ("call-1", Some("fs_read")),
    );
    assert_eq!(
        (call.kind, call.status),
        (AcpToolKind::Read, ToolCallStatus::Pending)
    );
    assert_eq!(
        (
            call.raw_input,
            call.meta
                .as_ref()
                .and_then(|meta| meta.get(HARNX_MARKDOWN_META))
        ),
        (
            Some(serde_json::json!({"path": "/tmp/config"})),
            Some(&serde_json::Value::String("Reading config".to_string())),
        ),
    );
}

macro_rules! lifecycle_case {
    (update $id:literal, $markdown:literal) => {
        (
            AgentEvent::Tool(ToolEvent::Update {
                id: $id.into(),
                markdown: Some($markdown.into()),
                status: None,
                content: None,
                title: None,
                kind: None,
                locations: None,
                usage: None,
            }),
            ExpectedToolUpdate {
                id: $id,
                status: ToolCallStatus::InProgress,
                output: None,
                text: Some($markdown),
            },
        )
    };
    (completed $id:literal, $output:expr, $expected_output:expr, $markdown:expr, $text:literal) => {
        (
            AgentEvent::Tool(ToolEvent::Completed {
                id: $id.into(),
                output: $output,
                markdown: $markdown.map(|markdown: &str| markdown.into()),
            }),
            ExpectedToolUpdate {
                id: $id,
                status: ToolCallStatus::Completed,
                output: Some($expected_output),
                text: Some($text),
            },
        )
    };
    (progress $id:literal, $text:literal) => {
        (
            AgentEvent::Tool(ToolEvent::Progress {
                id: $id.into(),
                text: $text.into(),
            }),
            ExpectedToolUpdate {
                id: $id,
                status: ToolCallStatus::InProgress,
                output: None,
                text: Some($text),
            },
        )
    };
    (failed $id:literal, $text:literal) => {
        (
            AgentEvent::Tool(ToolEvent::Failed {
                id: $id.into(),
                error: $text.into(),
            }),
            ExpectedToolUpdate {
                id: $id,
                status: ToolCallStatus::Failed,
                output: None,
                text: Some($text),
            },
        )
    };
}

fn tool_lifecycle_test_cases() -> Vec<(AgentEvent, ExpectedToolUpdate<'static>)> {
    let output = serde_json::json!({"files": 3});
    vec![
        lifecycle_case!(update "call-1", "Reading file 2 of 3"),
        lifecycle_case!(completed "call-1", output.clone(), output.clone(), Some("Read 3 files"), "Read 3 files"),
        lifecycle_case!(progress "call-progress", "2 of 4 files"),
        lifecycle_case!(failed "call-failed", "permission denied"),
        lifecycle_case!(completed "call-json", output.clone(), output, None, "{\"files\":3}"),
    ]
}

#[test]
fn tool_lifecycle_updates_map_correctly() {
    for (event, expected) in tool_lifecycle_test_cases() {
        let update = agent_event_to_session_update(event).expect("tool update should map");
        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected tool call update");
        };
        assert_tool_update(&update, expected);
    }
}

#[test]
fn model_error_has_error_metadata() {
    let update = agent_event_to_session_update(AgentEvent::Model(ModelEvent::Error(
        "provider unavailable".to_string(),
    )))
    .expect("model error should map");

    assert_eq!(agent_message_text(&update), Some("provider unavailable"));
    let SessionUpdate::AgentMessageChunk(chunk) = update else {
        panic!("expected agent message chunk");
    };
    assert_eq!(
        chunk
            .meta
            .as_ref()
            .and_then(|meta| meta.get(HARNX_ERROR_META)),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn warning_and_error_notices_map_but_info_and_empty_notices_do_not() {
    let warning = agent_event_to_session_update(AgentEvent::Notice(NoticeEvent::Warning(
        "retrying".to_string(),
    )))
    .expect("warning should map");
    let error = agent_event_to_session_update(AgentEvent::Notice(NoticeEvent::Error(
        "server stopped".to_string(),
    )))
    .expect("error should map");

    assert_eq!(
        (agent_message_text(&warning), agent_message_text(&error)),
        (Some("⚠ retrying"), Some("🔴 server stopped")),
    );
    assert!(
        agent_event_to_session_update(AgentEvent::Notice(NoticeEvent::Info(
            "presentation only".to_string()
        )))
        .is_none()
    );
    assert!(
        agent_event_to_session_update(AgentEvent::Notice(NoticeEvent::Warning(String::new())))
            .is_none()
    );
}

#[test]
fn replay_maps_user_and_final_agent_text_but_not_control_entries() {
    let user = agent_event_to_replay_update(
        AgentEvent::User(UserEvent::Message {
            content: "question".to_string(),
        }),
        "prod",
    )
    .expect("user replay should map");
    let agent = agent_event_to_replay_update(
        AgentEvent::Model(ModelEvent::Final {
            output: "answer".to_string(),
            usage: Default::default(),
        }),
        "prod",
    )
    .expect("agent replay should map");
    let control = agent_event_to_replay_update(
        AgentEvent::Session(SessionEvent::HandoffCommitted {
            agent: "atlas@prod".to_string(),
            session_id: "target".to_string(),
            handoff_tool_call_id: Some("handoff-call".to_string()),
            after_seq: Some(42),
        }),
        "prod",
    );

    assert_eq!(replay_message(&user), ("question", true));
    assert_eq!(replay_message(&agent), ("answer", false));
    assert!(control.is_none());
}

#[test]
fn requested_handoff_is_informational_only() {
    let update = agent_event_to_session_update_for_cluster(
        AgentEvent::Turn(TurnEvent::HandoffRequested {
            agent: "atlas@prod".to_string(),
            session_id: Some("tentative-target".to_string()),
        }),
        "source",
    );

    assert!(update.is_none());
}

#[test]
fn committed_handoff_maps_to_actionable_fallback() {
    let update = agent_event_to_session_update_for_cluster(
        AgentEvent::Session(SessionEvent::HandoffCommitted {
            agent: "atlas@prod".to_string(),
            session_id: "target-1".to_string(),
            handoff_tool_call_id: Some("handoff-call".to_string()),
            after_seq: Some(42),
        }),
        "source",
    )
    .expect("committed handoff should map");
    let message = agent_message_text(&update).expect("fallback should be agent text");

    for expected in [
        "agent `atlas`",
        "local session `target-1`",
        "cluster `prod`",
        "running independently",
        ".session atlas@prod target-1",
    ] {
        assert!(
            message.contains(expected),
            "missing `{expected}`: {message}"
        );
    }
}

#[test]
fn nested_sub_agent_commit_does_not_redirect_parent() {
    let update = agent_event_to_session_update_for_cluster(
        AgentEvent::SubAgent {
            source: AgentSource::default(),
            event: Box::new(AgentEvent::Session(SessionEvent::HandoffCommitted {
                agent: "atlas@prod".to_string(),
                session_id: "target-1".to_string(),
                handoff_tool_call_id: None,
                after_seq: Some(42),
            })),
        },
        "source",
    );

    assert!(update.is_none());
}

#[test]
fn sub_agent_events_are_translated_recursively() {
    let update = agent_event_to_session_update(AgentEvent::SubAgent {
        source: harnx_core::event::AgentSource::default(),
        event: Box::new(AgentEvent::Notice(NoticeEvent::Warning(
            "child warning".to_string(),
        ))),
    })
    .expect("nested warning should map");

    assert_eq!(agent_message_text(&update), Some("⚠ child warning"));
}

#[test]
fn thought_chunk_maps_to_agent_thought_chunk() {
    let update = agent_event_to_session_update(AgentEvent::Model(ModelEvent::ThoughtChunk {
        blocks: vec![ContentBlock::Text("checking files".to_string())],
    }))
    .expect("thought should map");

    let SessionUpdate::AgentThoughtChunk(chunk) = update else {
        panic!("expected agent thought chunk");
    };
    let AcpContentBlock::Text(text) = chunk.content else {
        panic!("expected text thought");
    };
    assert_eq!(text.text, "checking files");
}

#[test]
fn blocked_tool_maps_to_self_contained_failed_call() {
    let input = serde_json::json!({"path": "/root/secret"});
    let update = agent_event_to_session_update(AgentEvent::Tool(ToolEvent::Blocked {
        id: String::new(),
        name: "fs_read".to_string(),
        input: input.clone(),
        reason: "blocked by policy".to_string(),
    }))
    .expect("blocked tool should map");

    let SessionUpdate::ToolCall(call) = update else {
        panic!("expected self-contained tool call");
    };
    assert_eq!(
        (
            call.tool_call_id.0.as_ref(),
            call.name.as_deref(),
            call.status
        ),
        ("fs_read", Some("fs_read"), ToolCallStatus::Failed),
    );
    assert_eq!(call.raw_input, Some(input));
    let ToolCallContent::Content(content) = &call.content[0] else {
        panic!("expected tool text content");
    };
    let AcpContentBlock::Text(text) = &content.content else {
        panic!("expected blocked reason text");
    };
    assert_eq!(text.text, "blocked by policy");
}

#[test]
fn tool_started_empty_id_uses_name_and_maps_locations() {
    let update = agent_event_to_session_update(AgentEvent::Tool(ToolEvent::Started {
        id: String::new(),
        name: "fs_read".to_string(),
        kind: ToolKind::Read,
        markdown: None,
        input: serde_json::json!({}),
        locations: vec![harnx_core::event::ToolLocation {
            path: "/tmp/config".into(),
            line: Some(9),
        }],
    }))
    .expect("tool start should map");

    let SessionUpdate::ToolCall(call) = update else {
        panic!("expected tool call");
    };
    assert_eq!(
        (call.tool_call_id.0.as_ref(), call.locations.len()),
        ("fs_read", 1)
    );
    assert_eq!(
        (call.locations[0].path.clone(), call.locations[0].line),
        (std::path::PathBuf::from("/tmp/config"), Some(9)),
    );
}

#[test]
fn every_tool_status_maps_to_acp_status() {
    let cases = [
        (ToolStatus::Pending, ToolCallStatus::Pending),
        (ToolStatus::InProgress, ToolCallStatus::InProgress),
        (ToolStatus::Completed, ToolCallStatus::Completed),
        (ToolStatus::Failed, ToolCallStatus::Failed),
    ];
    for (harnx, acp) in cases {
        assert_eq!(map_tool_status(harnx), acp);
    }
}
