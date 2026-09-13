use super::*;

#[tokio::test]
async fn repeated_start_preserves_progress_and_open_view() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let key = monitored_key("researcher", "repeated-start");
    let snapshot = subagent_progress(&key, "inv-1", SubAgentProgressStatus::Running, 10_000);
    harness
        .tui()
        .record_subagent_progress(None, snapshot.clone());
    // Another delivery route can provide progress before the start announcement.
    emit_subagent_started(harness.tui(), &key).await;
    assert_eq!(
        harness.tui().app.monitored_sessions[&key]
            .invocation_id
            .as_deref(),
        Some("inv-1")
    );
    harness.tui().app.transcript_focus = Some(0);
    assert!(harness.tui().open_focused_root_subagent());

    emit_subagent_started(harness.tui(), &key).await;

    let [TranscriptItem::SubAgentSession {
        progress: Some(progress),
        ..
    }] = harness.tui().app.transcript.as_slice()
    else {
        panic!("expected a single progress row");
    };
    assert_eq!(progress.snapshot, snapshot);
    assert_eq!(
        harness.tui().app.subagent_view_stack[0]
            .progress
            .as_ref()
            .unwrap()
            .snapshot,
        snapshot
    );
}
