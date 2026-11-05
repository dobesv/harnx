//! Live log-seq binding for transcript rows (issue #2557: tool calls lacked
//! seqs, so `d`/`e` on them silently did nothing).

use super::*;
use harnx_core::event::TurnEvent;

#[tokio::test]
async fn log_seq_backfill_into_user_text_clears_pending_tool_seq() {
    // A UserText backfill consumes the seq: no same-round tool calls can
    // follow it, so pending_tool_seq must be cleared.
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.app
        .transcript
        .push(row(&RowSpec::LiveUser { text: "live user" }));
    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Session(
        SessionEvent::LogSeqAssigned { seq: 4 },
    )))
    .await
    .unwrap();

    assert_eq!(
        tui.app.pending_tool_seq, None,
        "pending_tool_seq must be None after a UserText backfill consumed the seq"
    );
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { seq: Some(4), text, .. } if text == "live user"
    )));
}

#[tokio::test]
async fn log_seq_backfill_into_assistant_text_keeps_pending_tool_seq() {
    // A ToolCalls log entry holds the assistant prose and the calls in one
    // row, so an AssistantText backfill keeps the seq pending for the same
    // round's ToolCall items (issue #2557: `d`/`e` on a tool call no-oped).
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Model(
        ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("assistant reply".to_string())],
        },
    )))
    .await
    .unwrap();

    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Session(
        SessionEvent::LogSeqAssigned { seq: 3 },
    )))
    .await
    .unwrap();

    assert_eq!(
        tui.app.pending_tool_seq,
        Some(3),
        "pending_tool_seq must stay set so the same round's ToolCalls share the seq"
    );
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::AssistantText { seq: Some(3), text, .. } if text == "assistant reply"
    )));
}

#[tokio::test]
async fn live_log_seq_assigned_after_streamed_text_shares_seq_with_tool_call() {
    // Live tool rounds stream the assistant prose first, then emit
    // LogSeqAssigned for the whole ToolCalls entry, then start the tools.
    // Both the prose and the calls belong to the same log row.
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Model(
        ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("let me check that".to_string())],
        },
    )))
    .await
    .unwrap();

    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Session(
        SessionEvent::LogSeqAssigned { seq: 9 },
    )))
    .await
    .unwrap();

    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Tool(ToolEvent::Started {
        id: "call-1".into(),
        name: "bash_exec".into(),
        kind: ToolKind::Other,
        markdown: None,
        input: yaml_to_json("command: echo hi"),
        locations: vec![],
    })))
    .await
    .unwrap();

    let assistant_text_seq = tui
        .app
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::AssistantText { text, seq, .. } if text == "let me check that" => {
                Some(*seq)
            }
            _ => None,
        })
        .expect("expected AssistantText transcript item");
    assert_eq!(assistant_text_seq, Some(9));

    let tool_call_seq = tui
        .app
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolCall { tool_name, seq, .. } if tool_name == "bash_exec" => {
                Some(*seq)
            }
            _ => None,
        })
        .expect("expected ToolCall transcript item");
    assert_eq!(tool_call_seq, Some(9));
}

#[tokio::test]
async fn log_seq_backfill_stops_at_sequenced_user_text() {
    // A non-streaming worker can leave an earlier turn's reply unsequenced.
    // The current turn's sequenced UserText is the turn boundary: the backfill
    // must stop there instead of assigning the new round's seq to the stale
    // reply (CodeRabbit review of PR #2622).
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.app.transcript.push(row(&RowSpec::LiveAssistant {
        text: "older reply",
    }));
    tui.app.transcript.push(row(&RowSpec::User {
        text: "new prompt",
        seq: Some(1),
    }));

    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Session(
        SessionEvent::LogSeqAssigned { seq: 2 },
    )))
    .await
    .unwrap();

    assert!(
        !tui.app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::AssistantText { seq: Some(2), text, .. } if text == "older reply"
        )),
        "the stale reply from an earlier turn must not consume the new round's seq"
    );
    assert_eq!(
        tui.app.pending_tool_seq,
        Some(2),
        "the seq stays pending for the current round's tool calls"
    );
}

#[tokio::test]
async fn turn_end_clears_pending_tool_seq() {
    // Hardening: a finished turn has no more tool calls to bind, so a seq
    // left pending must not leak into a later round.
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.pending_tool_seq = Some(7);

    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Turn(
        TurnEvent::Interrupted {
            cancellation_id: "cancel-1".into(),
        },
    )))
    .await
    .unwrap();

    assert_eq!(tui.app.pending_tool_seq, None);
}
