use super::*;
use harnx_core::event::{TurnEvent, TurnOutcome};

const PR: &str = "https://github.com/dobesv/harnx/pull/2385";
const INTERMEDIATE: &str =
    "PR5 branch pushed: [Open pull request](https://github.com/dobesv/harnx/pull/2385).";

fn chunk(text: &str) -> AgentEvent {
    AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text(text.into())],
    })
}

fn final_text(text: &str) -> AgentEvent {
    AgentEvent::Model(ModelEvent::Final {
        output: text.into(),
        usage: Default::default(),
    })
}

fn call(id: &str) -> AgentEvent {
    AgentEvent::Tool(ToolEvent::Started {
        id: id.into(),
        name: "bash_wait_for_pr_stable".into(),
        kind: ToolKind::Other,
        markdown: None,
        input: Value::Null,
        locations: vec![],
    })
}

fn reply_rows(items: &[TranscriptItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            TranscriptItem::AssistantText { text, .. } => format!("assistant:{text}"),
            TranscriptItem::MarkdownLink { text, url } => format!("link:{text}:{url}"),
            TranscriptItem::ToolCall { tool_name, .. } => format!("call:{tool_name}"),
            TranscriptItem::ToolResultMarkdown { text, .. } => format!("result:{text}"),
            other => panic!("unexpected row: {other:?}"),
        })
        .collect()
}

fn assert_intermediate(items: &[TranscriptItem]) {
    assert_eq!(
        message_rows(&items[..2]),
        [
            format!("assistant:{INTERMEDIATE}"),
            format!("link:Open pull request:{PR}")
        ]
    );
    assert!(items[1].is_navigable());
    let count = items
        .iter()
        .filter(|item| matches!(item, TranscriptItem::MarkdownLink { url, .. } if url == PR))
        .count();
    assert_eq!(count, 1);
}

fn replay_round(text: &str, ids: &[&str]) -> Message {
    let results = ids
        .iter()
        .map(|id| {
            ToolResult::new(
                ToolCall::new(
                    "bash_wait_for_pr_stable".into(),
                    Value::Null,
                    Some((*id).into()),
                    None,
                ),
                json!("done"),
            )
        })
        .collect();
    Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(MessageContentToolCalls::new(results, text.into(), None)),
    )
}

#[tokio::test]
async fn intermediate_pr_link_is_accessible_before_main_and_child_tool_results() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    for part in [
        "PR5 branch pushed: [Open pull",
        " request](https://github.com/dobesv/",
        "harnx/pull/2385).",
    ] {
        tui.render_agent_event(chunk(part)).await;
        apply_child_event(&mut child, chunk(part));
    }
    assert_eq!(
        tui.app.transcript.len(),
        1,
        "chunks must remain one parser input"
    );
    tui.render_agent_event(call("wait")).await;
    apply_child_event(&mut child, call("wait"));
    assert_intermediate(&tui.app.transcript);
    assert_intermediate(&child.transcript);
    assert!(
        matches!(&tui.app.transcript[2], TranscriptItem::ToolCall { tool_name, .. } if tool_name == "bash_wait_for_pr_stable")
    );
    assert_eq!(
        (tui.app.main_streamed_text_idx, child.streamed_text_idx),
        (None, None)
    );
    assert_eq!(
        reply_rows(&child.transcript),
        reply_rows(&tui.app.transcript)
    );
    tui.app.transcript_focus = Some(0);
    tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.app.transcript_focus, Some(1));
}

#[tokio::test]
async fn intermediate_multiple_tool_rounds_and_final_match_replay() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    let mut messages = Vec::new();
    for (text, ids) in [
        (INTERMEDIATE, vec!["a", "b"]),
        ("Next [Status](cid:status)", vec!["c"]),
    ] {
        tui.render_agent_event(chunk(text)).await;
        apply_child_event(&mut child, chunk(text));
        // A batch creates one link block, not one copy per Started event.
        for id in &ids {
            tui.render_agent_event(call(id)).await;
            apply_child_event(&mut child, call(id));
            tui.render_agent_event(completed(id, json!("done"), None))
                .await;
            apply_child_event(&mut child, completed(id, json!("done"), None));
        }
        messages.push(replay_round(text, &ids));
        let replay = messages_to_transcript_items(&messages, &HashMap::new());
        assert_eq!(reply_rows(&tui.app.transcript), reply_rows(&replay));
        assert_eq!(reply_rows(&child.transcript), reply_rows(&replay));
    }
    let output = "Final [Summary](cid:summary)";
    tui.render_agent_event(chunk("Final [Summary](cid:sum"))
        .await;
    tui.render_agent_event(chunk("mary)")).await;
    apply_child_event(&mut child, chunk(output));
    tui.render_agent_event(final_text(output)).await;
    apply_child_event(&mut child, final_text(output));
    messages.push(Message::new(
        MessageRole::Assistant,
        MessageContent::Text(output.into()),
    ));
    let replay = messages_to_transcript_items(&messages, &HashMap::new());
    assert_eq!(reply_rows(&tui.app.transcript), reply_rows(&replay));
    assert_eq!(reply_rows(&child.transcript), reply_rows(&replay));
    assert_intermediate(&tui.app.transcript);
    assert!(!tui.app.streaming_open && !child.streaming_open);
}

fn ending_boundaries() -> Vec<AgentEvent> {
    vec![
        call("a"),
        AgentEvent::Tool(ToolEvent::Blocked {
            id: "blocked".into(),
            name: "bash_wait_for_pr_stable".into(),
            input: Value::Null,
            reason: "denied".into(),
        }),
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: TurnOutcome {
                output: String::new(),
                thought: None,
                usage: Default::default(),
                handoff: None,
            },
        }),
        AgentEvent::Turn(TurnEvent::Interrupted {
            cancellation_id: "cancel".into(),
        }),
        AgentEvent::Turn(TurnEvent::Started),
        AgentEvent::Model(ModelEvent::Error("model failed".into())),
    ]
}

#[tokio::test]
async fn intermediate_round_boundaries_dedup_and_prevent_final_overwrite() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    for boundary in ending_boundaries() {
        tui.clear_transcript();
        let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
        tui.render_agent_event(chunk(INTERMEDIATE)).await;
        apply_child_event(&mut child, chunk(INTERMEDIATE));
        for _ in 0..3 {
            tui.render_agent_event(boundary.clone()).await;
            apply_child_event(&mut child, boundary.clone());
        }
        assert_intermediate(&tui.app.transcript);
        assert_intermediate(&child.transcript);
        tui.render_agent_event(final_text("later final")).await;
        apply_child_event(&mut child, final_text("later final"));
        assert_intermediate(&tui.app.transcript);
        assert_intermediate(&child.transcript);
        assert!(
            matches!(tui.app.transcript.last(), Some(TranscriptItem::AssistantText { text, .. }) if text == "later final")
        );
        assert!(
            matches!(child.transcript.last(), Some(TranscriptItem::AssistantText { text, .. }) if text == "later final")
        );
    }
}

#[tokio::test]
async fn intermediate_split_commonmark_syntax_uses_complete_stream() {
    let markdown = "[Inline](https://inline.example) [Reference][ref] <https://auto.example> [Again](https://inline.example)\n\n`[Code](https://code.example)`\n\n```text\n[Not a link](https://fenced.example)\n```\n\n[ref]: cid:reference";
    let expected = vec![
        "assistant:".to_owned() + markdown,
        "link:Inline:https://inline.example".into(),
        "link:Reference:cid:reference".into(),
        "link:https://auto.example:https://auto.example".into(),
        "call:bash_wait_for_pr_stable".into(),
    ];
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    for part in markdown.as_bytes().chunks(7) {
        let text = std::str::from_utf8(part).unwrap();
        tui.render_agent_event(chunk(text)).await;
        apply_child_event(&mut child, chunk(text));
        tui.render_agent_event(AgentEvent::Model(ModelEvent::Usage {
            input: 1,
            output: 1,
            cached: 0,
            cache_write: 0,
            session_label: None,
        }))
        .await;
    }
    tui.render_agent_event(call("syntax")).await;
    apply_child_event(&mut child, call("syntax"));
    assert_eq!(reply_rows(&tui.app.transcript), expected);
    assert_eq!(reply_rows(&child.transcript), expected);
}

#[tokio::test]
async fn intermediate_non_tail_canonical_final_preserves_metadata_and_selections() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    tui.render_agent_event(chunk(INTERMEDIATE)).await;
    tui.render_agent_event(AgentEvent::Session(SessionEvent::LogSeqAssigned {
        seq: 42,
    }))
    .await;
    let before = tui.app.transcript[0].clone();
    tui.render_agent_event(AgentEvent::Notice(NoticeEvent::Warning(
        "later status".into(),
    )))
    .await;
    assert_intermediate(&tui.app.transcript);
    tui.app.transcript_focus = Some(2);
    tui.app.transcript_selection_anchor = Some(1);
    let output = format!("[Extra](cid:extra) {INTERMEDIATE}");
    tui.render_agent_event(final_text(&output)).await;
    assert_eq!(
        (
            tui.app.transcript_focus,
            tui.app.transcript_selection_anchor
        ),
        (Some(3), Some(2)),
        "same URL, new ordered link position"
    );
    assert!(
        matches!((&before, &tui.app.transcript[0]), (TranscriptItem::AssistantText { timestamp: old, .. }, TranscriptItem::AssistantText { seq: Some(42), timestamp: new, rendered_cache: None, .. }) if old == new)
    );
    assert!(
        matches!(&tui.app.transcript[3], TranscriptItem::SystemText(text) if text.contains("later status"))
    );
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    apply_child_event(&mut child, chunk(INTERMEDIATE));
    if let TranscriptItem::AssistantText { seq, .. } = &mut child.transcript[0] {
        *seq = Some(42);
    }
    apply_child_event(
        &mut child,
        AgentEvent::Notice(NoticeEvent::Warning("later status".into())),
    );
    child.transcript_focus = Some(2);
    let child_before = child.transcript[0].clone();
    apply_child_event(&mut child, final_text(&output));
    assert_eq!(child.transcript_focus, Some(3));
    assert!(
        matches!((&child_before, &child.transcript[0]), (TranscriptItem::AssistantText { timestamp: old, .. }, TranscriptItem::AssistantText { seq: Some(42), timestamp: new, .. }) if old == new)
    );
    assert_eq!(
        message_rows(&tui.app.transcript),
        message_rows(&child.transcript)
    );
}

#[tokio::test]
async fn intermediate_source_switch_keeps_parent_final_target() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    tui.render_agent_event(chunk(INTERMEDIATE)).await;
    let source = AgentSource {
        agent: "delegate".into(),
        session_id: Some("child".into()),
        model: None,
    };
    tui.render_agent_event(AgentEvent::sub_agent(
        source.clone(),
        chunk("[Child](cid:child)"),
    ))
    .await;
    assert_intermediate(&tui.app.transcript);
    assert_eq!(tui.app.main_streamed_text_idx, Some(0));
    tui.render_agent_event(AgentEvent::sub_agent(source.clone(), call("child-tool")))
        .await;
    assert_eq!(
        tui.app.main_streamed_text_idx,
        Some(0),
        "child tool must not retire parent's canonical target"
    );
    let child_index = tui
        .app
        .transcript
        .iter()
        .position(
            |item| matches!(item, TranscriptItem::MarkdownLink { url, .. } if url == "cid:child"),
        )
        .unwrap();
    tui.app.transcript_focus = Some(child_index);
    tui.app.transcript_selection_anchor = Some(1);
    tui.render_agent_event(final_text("[Parent](cid:parent)"))
        .await;
    assert_eq!(
        message_rows(&[
            tui.app.transcript[0].clone(),
            tui.app.transcript[child_index].clone()
        ]),
        ["assistant:[Parent](cid:parent)", "link:Child:cid:child"]
    );
    assert_eq!(
        (
            tui.app.transcript_focus,
            tui.app.transcript_selection_anchor
        ),
        (Some(child_index), Some(0)),
        "removed URL falls back to its owning reply"
    );
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::MarkdownLink { url, .. } if url == PR)));
}

#[tokio::test]
async fn intermediate_thought_transition_keeps_true_thoughts_plain() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    tui.render_agent_event(chunk(INTERMEDIATE)).await;
    apply_child_event(&mut child, chunk(INTERMEDIATE));
    let thought = AgentEvent::Model(ModelEvent::ThoughtChunk {
        blocks: vec![ContentBlock::Text(
            "[Thought](https://thought.example)".into(),
        )],
    });
    tui.render_agent_event(thought.clone()).await;
    apply_child_event(&mut child, thought);
    assert_intermediate(&tui.app.transcript);
    assert_intermediate(&child.transcript);
    assert_eq!(tui.app.main_streamed_text_idx, Some(0));
    tui.render_agent_event(final_text(INTERMEDIATE)).await;
    apply_child_event(&mut child, final_text(INTERMEDIATE));
    assert_eq!((tui.app.transcript.len(), child.transcript.len()), (3, 3));
    tui.render_agent_event(call("thought")).await;
    apply_child_event(&mut child, call("thought"));
    assert!(
        matches!(&tui.app.transcript[2], TranscriptItem::ThoughtText(text) if text.contains("https://thought.example"))
    );
    assert!(
        matches!(&child.transcript[2], TranscriptItem::ThoughtText(text) if text.contains("https://thought.example"))
    );
    assert!(!tui.app.transcript.iter().any(|item| matches!(item, TranscriptItem::MarkdownLink { url, .. } if url.contains("thought.example"))));
}

#[test]
fn intermediate_link_refresh_rebases_all_indices_and_keeps_url_targets() {
    let mut transcript = vec![
        TranscriptItem::AssistantText {
            text: "[A](cid:a) [B](cid:b)".into(),
            seq: Some(2),
            timestamp: None,
            rendered_cache: None,
        },
        TranscriptItem::SystemText("later".into()),
    ];
    let mut focus = Some(1);
    let mut anchor = Some(0);
    let mut main = Some(1);
    let mut stream = Some(1);
    crate::assistant_transcript::refresh_assistant_links(
        &mut transcript,
        0,
        [&mut focus, &mut anchor, &mut main, &mut stream],
    );
    assert_eq!(
        (focus, anchor, main, stream),
        (Some(3), Some(0), Some(3), Some(3))
    );
    focus = Some(2);
    anchor = Some(1);
    crate::assistant_transcript::replace_or_append_assistant(
        &mut transcript,
        Some(0),
        "[B](cid:b)".into(),
    );
    crate::assistant_transcript::refresh_assistant_links(
        &mut transcript,
        0,
        [&mut focus, &mut anchor, &mut main, &mut stream],
    );
    assert_eq!(
        (focus, anchor, main, stream),
        (Some(1), Some(0), Some(2), Some(2))
    );
    for _ in 0..3 {
        crate::assistant_transcript::refresh_assistant_links(
            &mut transcript,
            0,
            [&mut focus, &mut anchor, &mut main, &mut stream],
        );
    }
    assert_eq!(transcript.len(), 3);
    assert_eq!(
        (focus, anchor, main, stream),
        (Some(1), Some(0), Some(2), Some(2))
    );
}

#[tokio::test]
async fn intermediate_tool_round_complete_and_task_fallback_finalize_links() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    for tool_round in [true, false] {
        tui.clear_transcript();
        tui.render_agent_event(chunk(INTERMEDIATE)).await;
        if tool_round {
            tui.handle_tui_event(TuiEvent::ToolRoundComplete)
                .await
                .unwrap();
        } else {
            tui.complete_main_prompt().await;
        }
        assert_intermediate(&tui.app.transcript);
        assert_eq!(tui.app.main_streamed_text_idx, None);
        tui.render_agent_event(final_text("next reply")).await;
        assert_intermediate(&tui.app.transcript);
        assert_eq!(tui.app.transcript.len(), 3);
    }
}

#[tokio::test]
async fn intermediate_parallel_tool_batch_exposes_one_reply_link_block() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    tui.render_agent_event(chunk(INTERMEDIATE)).await;
    apply_child_event(&mut child, chunk(INTERMEDIATE));
    for id in ["parallel-a", "parallel-b"] {
        tui.render_agent_event(call(id)).await;
        apply_child_event(&mut child, call(id));
    }
    assert_intermediate(&tui.app.transcript);
    assert_intermediate(&child.transcript);
    assert_eq!(
        reply_rows(&tui.app.transcript[2..]),
        [
            "call:bash_wait_for_pr_stable",
            "call:bash_wait_for_pr_stable"
        ]
    );
    for id in ["parallel-a", "parallel-b"] {
        tui.render_agent_event(completed(id, json!("done"), None))
            .await;
        apply_child_event(&mut child, completed(id, json!("done"), None));
    }
    let replay = messages_to_transcript_items(
        &[replay_round(INTERMEDIATE, &["parallel-a", "parallel-b"])],
        &HashMap::new(),
    );
    // Live batches group Started rows; replay pairs each result with its call.
    // The assistant block preceding either representation must be identical.
    assert_eq!(
        reply_rows(&tui.app.transcript[..2]),
        reply_rows(&replay[..2])
    );
    assert_eq!(
        reply_rows(&child.transcript),
        reply_rows(&tui.app.transcript)
    );
    assert_intermediate(&tui.app.transcript);
}

#[tokio::test]
async fn intermediate_empty_final_preserves_streamed_text_and_links() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    tui.render_agent_event(chunk(INTERMEDIATE)).await;
    apply_child_event(&mut child, chunk(INTERMEDIATE));
    tui.render_agent_event(final_text("")).await;
    apply_child_event(&mut child, final_text(""));
    assert_intermediate(&tui.app.transcript);
    assert_intermediate(&child.transcript);
    assert_eq!(
        reply_rows(&tui.app.transcript),
        reply_rows(&child.transcript)
    );
}

async fn seed_direct_terminal_stream(tui: &mut Tui) -> TranscriptItem {
    tui.clear_transcript();
    tui.app.llm_busy = true;
    for part in [
        "PR5 branch pushed: [Open pull request](https://github.com/",
        "dobesv/harnx/pull/2385).",
    ] {
        tui.render_agent_event(chunk(part)).await;
    }
    tui.render_agent_event(AgentEvent::Session(SessionEvent::LogSeqAssigned {
        seq: 73,
    }))
    .await;
    // A notice event would close the stream first and mask the direct-path bug.
    // Simulate a non-tail row added by another UI path while tracking stays open.
    tui.app
        .transcript
        .push(TranscriptItem::SystemText("later status".into()));
    assert!(tui.app.streaming_open);
    tui.app.transcript[0].clone()
}

fn assert_direct_terminal_projection(
    tui: &Tui,
    before: &TranscriptItem,
    expected: (Option<usize>, Option<usize>),
) {
    assert_intermediate(&tui.app.transcript);
    assert_eq!(
        format!("{:?}", tui.app.transcript[0]),
        format!("{before:?}"),
        "assistant text, sequence, timestamp and cache stay unchanged"
    );
    assert_eq!(
        (
            tui.app.transcript_focus,
            tui.app.transcript_selection_anchor
        ),
        expected
    );
    let status = &tui.app.transcript[2];
    assert!(matches!(status, TranscriptItem::SystemText(text) if text == "later status"));
    assert_eq!(
        (
            tui.app.streaming_open,
            tui.app.main_streamed_text_idx,
            tui.app.streamed_text_idx
        ),
        (false, None, None)
    );
    assert!(!tui.app.llm_busy);
}

#[tokio::test]
async fn intermediate_direct_ctrl_c_task_settlement_finalizes_before_reset() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    for (focus, anchor) in [
        (Some(1), Some(1)),
        (Some(0), Some(1)),
        (Some(1), Some(0)),
        (None, None),
    ] {
        let before = seed_direct_terminal_stream(&mut tui).await;
        tui.app.transcript_focus = focus;
        tui.app.transcript_selection_anchor = anchor;
        let task = harnx_runtime::utils::create_abort_signal();
        tui.current_prompt_abort = Some(task.clone());
        tui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(task.aborted());
        // Prompt-task exit after Ctrl+C calls settlement directly, not the
        // AgentEvent reducer's prepare_assistant_boundary path.
        tui.finish_prompt_task(task, None).await;
        let shifted = |index: Option<usize>| index.map(|i| if i > 0 { i + 1 } else { i });
        let expected = (shifted(focus), shifted(anchor));
        assert_direct_terminal_projection(&tui, &before, expected);
        tui.settle_interrupted_prompt();
        for _ in 0..3 {
            tui.render_agent_event(AgentEvent::Turn(TurnEvent::Interrupted {
                cancellation_id: "later receipt".into(),
            }))
            .await;
        }
        assert_direct_terminal_projection(&tui, &before, expected);
        assert_eq!(
            tui.app.transcript.len(),
            3,
            "repeat settlement and terminal advisories add no duplicate links"
        );
    }
}

#[tokio::test]
async fn intermediate_direct_task_error_fallback_finalizes_before_reset() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    for (focus, anchor) in [
        (Some(1), Some(1)),
        (Some(0), Some(1)),
        (Some(1), Some(0)),
        (None, None),
    ] {
        let before = seed_direct_terminal_stream(&mut tui).await;
        tui.app.transcript_focus = focus;
        tui.app.transcript_selection_anchor = anchor;
        let task = harnx_runtime::utils::create_abort_signal();
        tui.current_prompt_abort = Some(task.clone());
        // No ModelEvent::Error: task fallback reaches finish_main_prompt_error
        // before complete_main_prompt can close the still-open stream.
        tui.finish_prompt_task(task, Some("task failed".into()))
            .await;
        let shifted = |index: Option<usize>| index.map(|i| if i > 0 { i + 1 } else { i });
        let expected = (shifted(focus), shifted(anchor));
        assert_direct_terminal_projection(&tui, &before, expected);
        assert!(
            matches!(tui.app.transcript.last(), Some(TranscriptItem::ErrorText(text)) if text == "task failed")
        );
        for _ in 0..3 {
            tui.render_agent_event(AgentEvent::Model(ModelEvent::Error(
                "later error advisory".into(),
            )))
            .await;
        }
        assert_direct_terminal_projection(&tui, &before, expected);
    }
}
