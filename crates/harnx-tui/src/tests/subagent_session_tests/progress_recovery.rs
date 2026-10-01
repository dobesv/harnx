use super::*;

#[tokio::test]
async fn running_heartbeat_keeps_monitor_row_and_open_view() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let key = monitored_key("researcher", "heartbeat");
    let tui = harness.tui();
    tui.record_subagent_progress(
        None,
        subagent_progress(&key, "inv-1", SubAgentProgressStatus::Running, 10_000),
    );
    tui.app.transcript_focus = Some(0);
    assert!(tui.open_focused_root_subagent());
    tui.app
        .monitored_sessions
        .get_mut(&key)
        .unwrap()
        .streaming_open = true;
    let monitor = tui.subagent_monitor_handles[&key].id();

    // The reporter keeps sending running snapshots while the call runs. Each
    // one updates the invocation's metrics and must leave the child's monitor
    // alone: replacing it would reload the child's log every heartbeat.
    let heartbeat = SubAgentProgress {
        tool_call_count: 5,
        ..subagent_progress(&key, "inv-1", SubAgentProgressStatus::Running, 20_000)
    };
    tui.record_subagent_progress(None, heartbeat.clone());
    assert_heartbeat_shown(tui, &heartbeat);
    assert_monitor_kept(tui, &key, monitor);

    // Rebuilding the transcript from history drops the row of a call that has
    // no result yet, so the next heartbeat opens a row again for the
    // invocation the monitor already follows.
    tui.clear_transcript();
    let heartbeat = SubAgentProgress {
        elapsed_ms: 30_000,
        ..heartbeat
    };
    tui.record_subagent_progress(None, heartbeat.clone());
    assert_heartbeat_shown(tui, &heartbeat);
    assert_monitor_kept(tui, &key, monitor);
}

fn assert_heartbeat_shown(tui: &crate::types::Tui, heartbeat: &SubAgentProgress) {
    let [TranscriptItem::SubAgentSession {
        progress: Some(progress),
        ..
    }] = tui.app.transcript.as_slice()
    else {
        panic!("expected a single progress row");
    };
    assert_eq!(&progress.snapshot, heartbeat);
    let view = tui.app.subagent_view_stack[0].progress.as_ref().unwrap();
    assert_eq!(&view.snapshot, heartbeat);
}

fn assert_monitor_kept(
    tui: &crate::types::Tui,
    key: &MonitoredSessionKey,
    monitor: tokio::task::Id,
) {
    let state = &tui.app.monitored_sessions[key];
    assert_eq!(state.invocation_id.as_deref(), Some("inv-1"));
    assert!(state.streaming_open, "the open child stream was reset");
    assert_eq!(
        tui.subagent_monitor_handles[key].id(),
        monitor,
        "the child's monitor was replaced"
    );
}

#[tokio::test]
async fn concurrent_invocations_leave_the_monitor_on_the_newest_started() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let key = monitored_key("researcher", "shared-child");
    let tui = harness.tui();
    for invocation_id in ["inv-a", "inv-b"] {
        tui.record_subagent_progress(
            None,
            subagent_progress(&key, invocation_id, SubAgentProgressStatus::Running, 0),
        );
    }
    assert_eq!(
        tui.app.monitored_sessions[&key].invocation_id.as_deref(),
        Some("inv-b")
    );
    tui.app
        .monitored_sessions
        .get_mut(&key)
        .unwrap()
        .streaming_open = true;
    let monitor = tui.subagent_monitor_handles[&key].id();

    // Two prompts to one child each have a reporter sending heartbeats.
    for elapsed_ms in [10_000, 20_000] {
        for invocation_id in ["inv-a", "inv-b"] {
            tui.record_subagent_progress(
                None,
                subagent_progress(
                    &key,
                    invocation_id,
                    SubAgentProgressStatus::Running,
                    elapsed_ms,
                ),
            );
            let state = &tui.app.monitored_sessions[&key];
            assert_eq!(
                state.invocation_id.as_deref(),
                Some("inv-b"),
                "a heartbeat from {invocation_id} moved the monitor"
            );
            assert!(
                state.streaming_open,
                "a heartbeat from {invocation_id} reset the stream"
            );
            assert_eq!(
                tui.subagent_monitor_handles[&key].id(),
                monitor,
                "a heartbeat from {invocation_id} replaced the monitor"
            );
        }
    }
}
