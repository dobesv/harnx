use super::*;

async fn chunk(tui: &mut Tui, text: &str) {
    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Model(
        ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text(text.into())],
        },
    )))
    .await
    .unwrap();
}

fn assert_streaming_reset(tui: &Tui) {
    assert!(!tui.app.streaming_open);
    assert_eq!(
        (tui.app.main_streamed_text_idx, tui.app.streamed_text_idx),
        (None, None)
    );
    assert_eq!(
        (
            tui.app.transcript_focus,
            tui.app.transcript_selection_anchor
        ),
        (None, None)
    );
}

fn assert_compacted_transcript(tui: &Tui) {
    assert_eq!(
        tui.app
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::CompactionMarker { .. }))
            .count(),
        1
    );
    let assistant: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::AssistantText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        assistant,
        ["Resumed answer"],
        "Final must not replace the marker or duplicate streamed text"
    );
    assert_eq!(tui.app.transcript.iter().filter(|item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Current question")).count(), 1);
}

#[tokio::test]
async fn mid_turn_compaction_rebuild_then_chunks_and_final_keep_streaming_once() {
    let config = test_config_with_mock_client_and_agent("test-agent", None);
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.llm_busy = true;
    chunk(&mut tui, "archived tool-round prose").await;
    tui.app.transcript_focus = Some(0);
    tui.app.transcript_selection_anchor = Some(0);
    seed_compressed_session_with_summary(&config);
    for event in [
        SessionEvent::CompactingStarted {
            compaction_id: None,
        },
        SessionEvent::CompactingCompleted {
            compaction_id: None,
            outcome: harnx_core::session::CompactOutcome::Compacted,
        },
    ] {
        tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Session(event)))
            .await
            .unwrap();
    }
    assert_streaming_reset(&tui);
    assert!(tui.app.llm_busy, "compaction isn't the end of the turn");
    chunk(&mut tui, "Resumed ").await;
    chunk(&mut tui, "answer").await;
    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Model(ModelEvent::Final {
        output: "Resumed answer".into(),
        usage: Default::default(),
    })))
    .await
    .unwrap();
    assert_compacted_transcript(&tui);
}
