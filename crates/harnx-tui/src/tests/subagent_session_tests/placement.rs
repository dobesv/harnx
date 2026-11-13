use super::*;
use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
use harnx_core::tool::{ToolCall, ToolResult};
use std::collections::HashMap;

fn restored_call(id: &str, output: serde_json::Value) -> Message {
    Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(MessageContentToolCalls::new(
            vec![ToolResult::new(
                ToolCall::new(
                    "session_prompt".into(),
                    serde_json::Value::Null,
                    Some(id.into()),
                    None,
                ),
                output,
            )],
            String::new(),
            None,
        )),
    )
}

#[tokio::test]
async fn reopened_progress_only_results_stay_with_their_calls() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.clear_transcript();
    let key = monitored_key("researcher", "reused-child");
    let snapshots = ["first", "second"].map(|id| SubAgentProgress {
        tool_call_id: Some(id.into()),
        ..subagent_progress(&key, id, SubAgentProgressStatus::Done, 10_000)
    });
    // Some durable results contain progress without the legacy sub_agent marker.
    let messages = snapshots
        .iter()
        .map(|snapshot| {
            restored_call(
                snapshot.tool_call_id.as_deref().unwrap(),
                serde_json::json!({"sub_agent_progress": snapshot}),
            )
        })
        .chain(std::iter::once(Message::new(
            MessageRole::Assistant,
            MessageContent::Text("later answer".into()),
        )))
        .collect::<Vec<_>>();
    tui.app.transcript = crate::lifecycle::messages_to_transcript_items_for_cluster(
        &messages,
        &HashMap::new(),
        Some(&key.cluster),
    );
    // Recover out of order, then replay again as on a reconnect.
    for snapshot in [&snapshots[1], &snapshots[0], &snapshots[1], &snapshots[0]] {
        tui.record_recovered_subagent_progress(snapshot.clone());
    }
    assert!(matches!(tui.app.transcript.as_slice(), [
        TranscriptItem::ToolCall { id: Some(first_call), .. },
        TranscriptItem::ToolResultMarkdown { .. },
        TranscriptItem::SubAgentSession { invocation_id: Some(first), .. },
        TranscriptItem::ToolCall { id: Some(second_call), .. },
        TranscriptItem::ToolResultMarkdown { .. },
        TranscriptItem::SubAgentSession { invocation_id: Some(second), .. },
        TranscriptItem::AssistantText { text, .. },
    ] if first_call == "first" && first == "first" && second_call == "second" && second == "second" && text == "later answer"));
}

#[tokio::test]
async fn archived_progress_updates_open_child_without_resurrecting_rows() {
    for tool_call_id in [Some("old-call".into()), None] {
        let mut harness = TuiTestHarness::new().await;
        let tui = harness.tui();
        let key = monitored_key("researcher", "archived-child");
        install_archived_view(tui, &key).await;
        tui.record_recovered_subagent_progress(SubAgentProgress {
            tool_call_id,
            ..subagent_progress(&key, "inv-1", SubAgentProgressStatus::Done, 12_345)
        });
        // A stale recovery preview must not reopen the archived child view.
        tui.record_recovered_subagent_progress(subagent_progress(
            &key,
            "inv-1",
            SubAgentProgressStatus::Running,
            1,
        ));
        assert!(matches!(tui.app.transcript.as_slice(), [
            TranscriptItem::CompactionMarker { .. },
            TranscriptItem::AssistantText { text, .. },
        ] if text == "kept answer"));
        let view = &tui.app.subagent_view_stack[0];
        assert_eq!(view.status, SubAgentStatus::Completed);
        assert_eq!(view.progress.as_ref().unwrap().elapsed_ms(), 12_345);
    }
}

async fn install_archived_view(tui: &mut crate::types::Tui, key: &MonitoredSessionKey) {
    tui.clear_transcript();
    emit_subagent_started(tui, key).await;
    tui.app.transcript_focus = Some(0);
    assert!(tui.open_focused_root_subagent());
    let archived = restored_call("old-call", serde_json::json!({}));
    let kept = Message::new(
        MessageRole::Assistant,
        MessageContent::Text("kept answer".into()),
    );
    tui.app.transcript = crate::lifecycle::build_transcript_with_compaction_for_cluster(
        &[archived],
        &[kept],
        Some("summary"),
        &HashMap::new(),
        Some(&key.cluster),
    );
}

#[tokio::test]
async fn late_child_updates_keep_root_and_nested_streaming_targets() {
    for nested in [false, true] {
        let mut harness = TuiTestHarness::new().await;
        let tui = harness.tui();
        tui.clear_transcript();
        let parent = nested.then(|| monitored_key("parent", "parent-session"));
        if let Some(parent) = &parent {
            emit_subagent_invocation_started(tui, parent, "parent-inv").await;
        }
        let key = monitored_key("researcher", "late-child");
        install_late_transcript(tui, parent.as_ref(), &key);
        tui.record_subagent_progress(
            parent.as_ref(),
            SubAgentProgress {
                tool_call_id: Some("delegate-call".into()),
                ..subagent_progress(&key, "inv-1", SubAgentProgressStatus::Running, 0)
            },
        );
        deliver(tui, parent.as_ref(), completed_subagent_event(&key)).await;
        // Re-delivery and late heartbeats cannot move the completed row.
        deliver(tui, parent.as_ref(), completed_subagent_event(&key)).await;
        tui.record_subagent_progress(
            parent.as_ref(),
            SubAgentProgress {
                tool_call_id: Some("delegate-call".into()),
                ..subagent_progress(&key, "inv-1", SubAgentProgressStatus::Running, 1)
            },
        );
        if parent.is_none() {
            assert_eq!(tui.app.transcript_selection_anchor, Some(6));
            assert_eq!(tui.app.main_streamed_text_idx, Some(6));
        }
        let (items, focus, streamed) = tracked_transcript(tui, parent.as_ref());
        assert_eq!(focus, Some(6));
        assert_eq!(streamed, Some(6));
        assert!(
            matches!(&items[4], TranscriptItem::ToolResultMarkdown { text, .. } if text == "child response")
        );
        assert!(matches!(
            &items[5],
            TranscriptItem::SubAgentSession {
                status: SubAgentStatus::Completed,
                ..
            }
        ));
        assert!(
            matches!(&items[6], TranscriptItem::AssistantText { text, .. } if text == "later stream")
        );
    }
}

async fn deliver(
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

fn install_late_transcript(
    tui: &mut crate::types::Tui,
    parent: Option<&MonitoredSessionKey>,
    key: &MonitoredSessionKey,
) {
    let messages = [restored_call("delegate-call", serde_json::json!("receipt"))];
    let mut transcript = crate::lifecycle::messages_to_transcript_items_for_cluster(
        &messages,
        &HashMap::new(),
        Some(&key.cluster),
    );
    // Keep call links attached to the call, and result links to the result.
    transcript.insert(
        1,
        TranscriptItem::MarkdownLink {
            text: "input".into(),
            url: "cid:input".into(),
        },
    );
    transcript.push(TranscriptItem::MarkdownLink {
        text: "receipt".into(),
        url: "cid:receipt".into(),
    });
    transcript.push(assistant_text("later stream"));
    if let Some(parent) = parent {
        let state = tui.app.monitored_sessions.get_mut(parent).unwrap();
        state.transcript = transcript;
        state.transcript_focus = Some(4);
        state.streamed_text_idx = Some(4);
    } else {
        tui.app.transcript = transcript;
        tui.app.transcript_focus = Some(4);
        tui.app.transcript_selection_anchor = Some(4);
        tui.app.main_streamed_text_idx = Some(4);
        tui.app.streamed_text_idx = Some(4);
    }
}

#[tokio::test]
async fn reply_redelivery_preserves_focused_links_in_root_and_child() {
    for nested in [false, true] {
        let mut harness = TuiTestHarness::new().await;
        let tui = harness.tui();
        tui.clear_transcript();
        let parent = nested.then(|| monitored_key("parent", "link-parent"));
        if let Some(parent) = &parent {
            emit_subagent_invocation_started(tui, parent, "parent-inv").await;
        }
        let key = monitored_key("researcher", "linked-child");
        let mut event = completed_subagent_event(&key);
        if let AgentEvent::Tool(ToolEvent::Completed { output, .. }) = &mut event {
            output["response"] = serde_json::json!("Read [link](cid:reply).");
        }
        deliver(tui, parent.as_ref(), event.clone()).await;
        if let Some(parent) = &parent {
            tui.app
                .monitored_sessions
                .get_mut(parent)
                .unwrap()
                .transcript_focus = Some(1);
        } else {
            tui.app.transcript_focus = Some(1);
            tui.app.transcript_selection_anchor = Some(1);
        }
        deliver(tui, parent.as_ref(), event).await;
        if parent.is_none() {
            assert_eq!(tui.app.transcript_selection_anchor, Some(1));
        }
        let (items, focus, _) = tracked_transcript(tui, parent.as_ref());
        assert_eq!(focus, Some(1));
        assert!(
            matches!(&items[1], TranscriptItem::MarkdownLink { url, .. } if url == "cid:reply")
        );
    }
}

fn tracked_transcript<'a>(
    tui: &'a crate::types::Tui,
    parent: Option<&MonitoredSessionKey>,
) -> (&'a [TranscriptItem], Option<usize>, Option<usize>) {
    match parent {
        Some(parent) => {
            let state = &tui.app.monitored_sessions[parent];
            (
                &state.transcript,
                state.transcript_focus,
                state.streamed_text_idx,
            )
        }
        None => (
            &tui.app.transcript,
            tui.app.transcript_focus,
            tui.app.streamed_text_idx,
        ),
    }
}

#[tokio::test]
async fn late_call_id_anchors_running_row_and_preserves_its_selection() {
    for nested in [false, true] {
        let mut harness = TuiTestHarness::new().await;
        let tui = harness.tui();
        tui.clear_transcript();
        let parent = nested.then(|| monitored_key("parent", "late-origin"));
        if let Some(parent) = &parent {
            emit_subagent_invocation_started(tui, parent, "parent-inv").await;
        }
        let key = monitored_key("researcher", "uncorrelated-child");
        install_late_transcript(tui, parent.as_ref(), &key);
        tui.record_subagent_progress(
            parent.as_ref(),
            subagent_progress(&key, "inv-1", SubAgentProgressStatus::Running, 0),
        );
        // The ID-less preview had to append after the later assistant text.
        if let Some(parent) = &parent {
            tui.app
                .monitored_sessions
                .get_mut(parent)
                .unwrap()
                .transcript_focus = Some(5);
        } else {
            tui.app.transcript_focus = Some(5);
            tui.app.transcript_selection_anchor = Some(5);
        }
        deliver(tui, parent.as_ref(), completed_subagent_event(&key)).await;
        if parent.is_none() {
            assert_eq!(tui.app.transcript_selection_anchor, Some(5));
            assert_eq!(tui.app.main_streamed_text_idx, Some(6));
        }
        let (items, focus, streamed) = tracked_transcript(tui, parent.as_ref());
        assert_eq!(focus, Some(5));
        assert_eq!(streamed, Some(6));
        assert!(matches!(
            &items[5],
            TranscriptItem::SubAgentSession {
                status: SubAgentStatus::Completed,
                ..
            }
        ));
        assert!(
            matches!(&items[6], TranscriptItem::AssistantText { text, .. } if text == "later stream")
        );
    }
}
