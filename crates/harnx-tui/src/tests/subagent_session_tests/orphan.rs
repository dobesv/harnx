use super::*;

#[tokio::test]
async fn stale_snapshot_cannot_replace_a_resumed_invocation() {
    let mut harness = TuiTestHarness::new().await;
    let key = monitored_key("athena", "snapshot-race");
    emit_subagent_invocation_started(harness.tui(), &key, Some("old")).await;
    harness.tui().fail_monitored_invocation(&key, "old");
    emit_subagent_invocation_started(harness.tui(), &key, Some("resumed")).await;
    for invocation_id in [Some("old".to_string()), None] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::SubAgentSessionSnapshot {
                key: key.clone(),
                snapshot: crate::types::SubAgentSnapshot {
                    invocation_id,
                    transcript: vec![TranscriptItem::ErrorText("stale failure".into())],
                    status: SubAgentStatus::Failed,
                },
            })
            .await
            .unwrap();
        let state = &harness.tui().app.monitored_sessions[&key];
        assert_eq!(state.status, SubAgentStatus::Running);
        assert!(state.transcript.is_empty());
    }
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionSnapshot {
            key: key.clone(),
            snapshot: crate::types::SubAgentSnapshot {
                invocation_id: Some("resumed".into()),
                transcript: vec![assistant_text("current result")],
                status: SubAgentStatus::Completed,
            },
        })
        .await
        .unwrap();
    assert_eq!(
        harness.tui().app.monitored_sessions[&key].status,
        SubAgentStatus::Completed
    );
}

#[tokio::test]
async fn orphaned_invocation_stops_spinning_without_failing_its_resumed_invocation() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let key = monitored_key("athena", "orphan-child");
    emit_subagent_invocation_started(harness.tui(), &key, Some("old-invocation")).await;
    harness.tui().app.transcript_focus = Some(0);
    assert!(harness.tui().open_focused_root_subagent());
    emit_subagent_invocation_started(harness.tui(), &key, Some("resumed-invocation")).await;
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentInvocationFailed {
            key: key.clone(),
            invocation_id: "old-invocation".into(),
        })
        .await
        .unwrap();
    let rows = &harness.tui().app.transcript;
    assert!(matches!(&rows[0], TranscriptItem::SubAgentSession {
        status: SubAgentStatus::Failed, progress: Some(progress), ..
    } if progress.snapshot.status == SubAgentProgressStatus::Failed));
    assert!(matches!(&rows[1], TranscriptItem::SubAgentSession {
        status: SubAgentStatus::Running, progress: Some(progress), ..
    } if progress.snapshot.invocation_id == "resumed-invocation"));
    let view = harness.tui().app.subagent_view_stack.last().unwrap();
    assert_eq!(view.status, SubAgentStatus::Failed);
    let elapsed = view.progress.as_ref().unwrap().elapsed_ms();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        harness
            .tui()
            .app
            .subagent_view_stack
            .last()
            .unwrap()
            .progress
            .as_ref()
            .unwrap()
            .elapsed_ms(),
        elapsed
    );
}
