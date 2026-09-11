use super::*;

#[tokio::test]
async fn completed_remote_path_orders_boundary_before_hydrated_handoff() {
    use harnx_core::session::SessionLogEntry;

    let thread_id = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4().to_string();
    let log_entries = vec![(
        1u64,
        SessionLogEntry::HandoffCommitted {
            target_agent: "remote-agent".to_string(),
            target_session_id: "remote-session-456".to_string(),
            handoff_tool_call_id: Some("call-remote".to_string()),
        },
    )];
    let control_frames = control_snapshot_events(&log_entries, None)
        .into_iter()
        .filter_map(|event| frame_event(&event).ok().map(Bytes::from))
        .collect();
    let initial_frames = [
        Bytes::from(frame_run_boundary_event("RUN_STARTED", &thread_id, &run_id)),
        crate::ag_ui_attach::session_attach_boundary_frame(1),
    ];
    let snapshot_frame = Some(Bytes::from(
        frame_event(&snapshot_event(vec![user_msg("remote test")])).expect("snapshot frame"),
    ));

    let stream = crate::ag_ui_remote_follow::completed_remote_stream(
        initial_frames,
        snapshot_frame,
        control_frames,
        &thread_id,
        &run_id,
    );
    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);

    assert_event_type_sequence(
        &events,
        &[
            "RUN_STARTED",
            "CUSTOM",
            "MESSAGES_SNAPSHOT",
            "CUSTOM",
            "RUN_FINISHED",
        ],
    );
    assert_attach_boundary(&events[1], 1);
    let handoff = &events[3];
    assert_eq!(handoff["name"], "session_handoff");
    assert_eq!(handoff["value"]["agent"], "remote-agent");
    assert_eq!(handoff["value"]["session_id"], "remote-session-456");
    assert_eq!(handoff["value"]["handoff_tool_call_id"], "call-remote");
    assert_eq!(handoff["value"]["after_seq"], 1);
}
