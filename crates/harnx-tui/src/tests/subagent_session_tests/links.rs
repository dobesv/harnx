use super::*;

const LINKED_REPLY: &str = "Read [First](https://first.example) and [Second](cid:second).";

fn linked_completion(key: &MonitoredSessionKey) -> AgentEvent {
    let mut event = completed_subagent_event(key);
    if let AgentEvent::Tool(ToolEvent::Completed {
        output, markdown, ..
    }) = &mut event
    {
        output["response"] = serde_json::json!(LINKED_REPLY);
        *markdown = Some(LINKED_REPLY.into());
    }
    event
}

fn assert_linked_reply_block(items: &[TranscriptItem]) {
    assert!(
        matches!(items, [
        TranscriptItem::ToolResultMarkdown { text, .. },
        TranscriptItem::MarkdownLink { text: first, url: first_url },
        TranscriptItem::MarkdownLink { text: second, url: second_url },
        TranscriptItem::SubAgentSession { status: SubAgentStatus::Completed, .. },
    ] if text == LINKED_REPLY && first == "First" && first_url == "https://first.example" && second == "Second" && second_url == "cid:second"),
        "{items:?}"
    );
}

#[tokio::test]
async fn live_subagent_linked_reply_preserves_order_dedup_and_history_parity() {
    let mut harness = TuiTestHarness::with_size(100, 24).await;
    let tui = harness.tui();
    tui.clear_transcript();
    let key = monitored_key("researcher", "linked-session");
    start_linked_tool(tui).await;
    emit_subagent_invocation_started(tui, &key, "inv-1").await;
    for _ in 0..3 {
        tui.handle_tui_event(TuiEvent::LocalAgent(linked_completion(&key)))
            .await
            .unwrap();
        assert_eq!(tui.app.transcript.len(), 6);
        assert!(
            matches!(&tui.app.transcript[1], TranscriptItem::MarkdownLink { url, .. } if url == "cid:prompt")
        );
        assert_linked_reply_block(&tui.app.transcript[2..]);
    }

    // A second invocation with the same reply must keep its own block.
    emit_subagent_invocation_started(tui, &key, "inv-2").await;
    let mut second = linked_completion(&key);
    if let AgentEvent::Tool(ToolEvent::Completed { output, .. }) = &mut second {
        output["sub_agent_progress"]["invocation_id"] = serde_json::json!("inv-2");
    }
    for event in [second.clone(), second, linked_completion(&key)] {
        tui.handle_tui_event(TuiEvent::LocalAgent(event))
            .await
            .unwrap();
        assert_eq!(tui.app.transcript.len(), 10);
        assert_linked_reply_block(&tui.app.transcript[2..6]);
        assert_linked_reply_block(&tui.app.transcript[6..]);
    }

    let history = linked_history(&key);
    assert_linked_reply_block(&history[1..]);
    assert_eq!(
        crate::tests::markdown_link_accessibility_tests::rows(&tui.app.transcript[2..6]),
        crate::tests::markdown_link_accessibility_tests::rows(&history[1..])
    );

    tui.app.transcript_focus = Some(0);
    let (entries, _) = crate::detail_view::detail_view_content(&tui.app);
    let detail = entries
        .iter()
        .flatten()
        .map(crate::tests::line_to_plain)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(detail.matches(LINKED_REPLY).count(), 1);
    harness.render();
    let screen = harness.screen_contents();
    assert!(screen.contains("https://first.example"), "{screen}");
    assert!(screen.contains("cid:second"), "{screen}");
}

#[tokio::test]
async fn nested_subagent_linked_reply_is_atomic_and_deduplicated() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.clear_transcript();
    let parent = monitored_key("researcher", "parent");
    let child = monitored_key("reviewer", "child");
    let stamp = child_event_stamp(tui, &parent);
    tui.handle_tui_event(TuiEvent::SubAgentSessionEvent {
        stamp,
        key: parent.clone(),
        event: AgentEvent::Turn(TurnEvent::SubAgentProgress(subagent_progress(
            &child,
            "inv-1",
            SubAgentProgressStatus::Running,
            0,
        ))),
    })
    .await
    .unwrap();
    for _ in 0..3 {
        let stamp = child_event_stamp(tui, &parent);
        tui.handle_tui_event(TuiEvent::SubAgentSessionEvent {
            stamp,
            key: parent.clone(),
            event: linked_completion(&child),
        })
        .await
        .unwrap();
        assert_linked_reply_block(&tui.app.monitored_sessions[&parent].transcript);
        assert!(tui.app.transcript.is_empty());
    }
}

#[tokio::test]
async fn linked_subagent_completion_without_prior_status_is_idempotent() {
    let mut tui = crate::types::Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let key = monitored_key("researcher", "orphan-linked");
    for _ in 0..3 {
        tui.handle_tui_event(TuiEvent::LocalAgent(linked_completion(&key)))
            .await
            .unwrap();
        assert_linked_reply_block(&tui.app.transcript);
    }
}

async fn start_linked_tool(tui: &mut crate::types::Tui) {
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Started {
        id: "delegate-call".into(),
        name: "session_prompt".into(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: Some("[Prompt](cid:prompt)".into()),
        input: serde_json::Value::Null,
        locations: vec![],
    }))
    .await;
}

fn linked_history(key: &MonitoredSessionKey) -> Vec<TranscriptItem> {
    use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
    use harnx_core::tool::{ToolCall, ToolResult};
    use std::collections::HashMap;

    let AgentEvent::Tool(ToolEvent::Completed { output, .. }) = linked_completion(key) else {
        unreachable!()
    };
    let call = ToolCall::new("session_prompt".into(), serde_json::Value::Null, None, None);
    let messages = vec![Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(MessageContentToolCalls::new(
            vec![ToolResult::new(call, output)],
            String::new(),
            None,
        )),
    )];
    crate::lifecycle::messages_to_transcript_items_for_cluster(
        &messages,
        &HashMap::new(),
        Some(&key.cluster),
    )
}

#[tokio::test]
async fn subagent_reply_does_not_replace_adjacent_equal_tool_result() {
    let mut tui = crate::types::Tui::init(&test_config()).await.unwrap();
    let key = monitored_key("researcher", "reply-owner");
    for nested in [false, true] {
        tui.clear_transcript();
        let parent = nested.then(|| monitored_key("parent", "parent-session"));
        let normal = AgentEvent::Tool(ToolEvent::Completed {
            id: "ordinary".into(),
            output: serde_json::json!(LINKED_REPLY),
            markdown: None,
        });
        deliver_reply_test_event(&mut tui, parent.as_ref(), normal).await;
        let running = AgentEvent::Turn(TurnEvent::SubAgentProgress(subagent_progress(
            &key,
            "inv-1",
            SubAgentProgressStatus::Running,
            0,
        )));
        deliver_reply_test_event(&mut tui, parent.as_ref(), running).await;
        for _ in 0..3 {
            deliver_reply_test_event(&mut tui, parent.as_ref(), linked_completion(&key)).await;
            let items = parent.as_ref().map_or(&tui.app.transcript, |parent| {
                &tui.app.monitored_sessions[parent].transcript
            });
            assert!(matches!(&items[0], TranscriptItem::ToolResultMarkdown {
                text, subagent_reply_owner: None, ..
            } if text == LINKED_REPLY));
            assert!(
                matches!(&items[1], TranscriptItem::MarkdownLink { url, .. } if url == "https://first.example")
            );
            assert!(
                matches!(&items[2], TranscriptItem::MarkdownLink { url, .. } if url == "cid:second")
            );
            assert_linked_reply_block(&items[3..]);
        }
    }
}

async fn deliver_reply_test_event(
    tui: &mut crate::types::Tui,
    parent: Option<&MonitoredSessionKey>,
    event: AgentEvent,
) {
    let event = match parent {
        Some(parent) => TuiEvent::SubAgentSessionEvent {
            stamp: child_event_stamp(tui, parent),
            key: parent.clone(),
            event,
        },
        None => TuiEvent::LocalAgent(event),
    };
    tui.handle_tui_event(event).await.unwrap();
}
