use super::*;

#[tokio::test]
async fn live_subagent_reply_appears_before_status_row() {
    let mut harness = TuiTestHarness::with_size(60, 12).await;
    let tui = harness.tui();
    tui.clear_transcript();
    let key = monitored_key("agentA", "sessionX");

    // 1) Tool started (prompt)
    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Tool(ToolEvent::Started {
        id: "delegate-call".into(),
        name: "agentA_session_prompt".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: serde_json::json!({"message": "do thing"}),
        locations: vec![],
    })))
    .await
    .unwrap();

    // Verify ToolCall is in transcript
    assert_eq!(tui.app.transcript.len(), 1);
    assert!(matches!(
        tui.app.transcript[0],
        TranscriptItem::ToolCall { .. }
    ));

    // 2) The child's first running snapshot (status row)
    emit_subagent_invocation_started(tui, &key, "inv-1").await;
    assert_eq!(tui.app.transcript.len(), 2);
    assert!(matches!(
        tui.app.transcript[1],
        TranscriptItem::SubAgentSession { .. }
    ));

    // For production fidelity, emit a standalone terminal TurnEvent::SubAgentProgress BEFORE the ToolEvent::Completed
    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Turn(
        TurnEvent::SubAgentProgress(subagent_progress(
            &key,
            "inv-1",
            SubAgentProgressStatus::Done,
            12_345,
        )),
    )))
    .await
    .unwrap();

    // 3) Tool completed (reply)
    tui.handle_tui_event(TuiEvent::LocalAgent(completed_subagent_event(&key)))
        .await
        .unwrap();

    // Verify order: ToolCall, ToolResultMarkdown, SubAgentSession
    assert_eq!(tui.app.transcript.len(), 3);
    assert!(matches!(
        tui.app.transcript[0],
        TranscriptItem::ToolCall { .. }
    ));
    assert!(matches!(
        &tui.app.transcript[1],
        TranscriptItem::ToolResultMarkdown { text, .. } if text == "child response"
    ));
    assert!(matches!(
        &tui.app.transcript[2],
        TranscriptItem::SubAgentSession {
            status: SubAgentStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn nested_subagent_reply_appears_in_parent_child_transcript() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let parent = monitored_key("researcher", "parent-session");
    let nested = monitored_key("reviewer", "nested-session");

    // Parent sub-agent session started.
    emit_subagent_invocation_started(harness.tui(), &parent, "parent-inv").await;

    // Send a sub-agent started (Progress) event for the nested session through the parent's TuiEvent wrapper.
    let stamp = child_event_stamp(harness.tui(), &parent);
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionEvent {
            stamp,
            key: parent.clone(),
            event: AgentEvent::sub_agent(
                harnx_core::event::AgentSource {
                    agent: nested.agent.clone(),
                    session_id: Some(nested.session_id.clone()),
                    model: None,
                },
                AgentEvent::Turn(TurnEvent::SubAgentProgress(subagent_progress(
                    &nested,
                    "inv-1", // Match completed_subagent_event's id
                    SubAgentProgressStatus::Running,
                    3_000,
                ))),
            ),
        })
        .await
        .unwrap();

    // Now send the completion tool event nested inside the parent's SubAgentSessionEvent.
    let stamp = child_event_stamp(harness.tui(), &parent);
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionEvent {
            stamp,
            key: parent.clone(),
            event: AgentEvent::sub_agent(
                harnx_core::event::AgentSource {
                    agent: nested.agent.clone(),
                    session_id: Some(nested.session_id.clone()),
                    model: None,
                },
                completed_subagent_event(&nested),
            ),
        })
        .await
        .unwrap();

    // Main transcript should only contain the parent's subagent session row
    assert_eq!(harness.tui().app.transcript.len(), 1);

    let parent_transcript = &harness.tui().app.monitored_sessions[&parent].transcript;
    // Parent's child transcript should contain the nested result and the nested subagent session
    assert_eq!(parent_transcript.len(), 2);

    assert!(matches!(
        &parent_transcript[0],
        TranscriptItem::ToolResultMarkdown { text, .. } if text == "child response"
    ));
    assert!(matches!(
        &parent_transcript[1],
        TranscriptItem::SubAgentSession {
            key,
            status: SubAgentStatus::Completed,
            ..
        } if key == &nested
    ));
}

#[tokio::test]
async fn live_subagent_empty_response_omits_reply_row() {
    let mut harness = TuiTestHarness::with_size(60, 12).await;
    let tui = harness.tui();
    tui.clear_transcript();
    let key = monitored_key("agentA", "sessionX");

    // 1) Tool started (prompt)
    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Tool(ToolEvent::Started {
        id: "delegate-call".into(),
        name: "agentA_session_prompt".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: serde_json::json!({"message": "do thing"}),
        locations: vec![],
    })))
    .await
    .unwrap();

    // 2) The child's first running snapshot (status row)
    emit_subagent_invocation_started(tui, &key, "inv-1").await;

    // 3) Tool completed (empty reply)
    let progress = subagent_progress(&key, "inv-1", SubAgentProgressStatus::Done, 12_345);
    let empty_response_event = AgentEvent::Tool(ToolEvent::Completed {
        id: "delegate-call".into(),
        output: serde_json::json!({
            "response": "",
            "content": [{"type": "text", "text": ""}],
            "sub_agent": {
                "agent": key.agent.clone(),
                "session_id": key.session_id.clone(),
            },
            "sub_agent_progress": serde_json::to_value(&progress).unwrap()
        }),
        markdown: Some("".into()),
    });

    tui.handle_tui_event(TuiEvent::LocalAgent(empty_response_event))
        .await
        .unwrap();

    // Verify order: ToolCall, SubAgentSession (No ToolResultMarkdown)
    assert_eq!(tui.app.transcript.len(), 2);
    assert!(matches!(
        tui.app.transcript[0],
        TranscriptItem::ToolCall { .. }
    ));
    assert!(matches!(
        &tui.app.transcript[1],
        TranscriptItem::SubAgentSession {
            status: SubAgentStatus::Completed,
            ..
        }
    ));
}
