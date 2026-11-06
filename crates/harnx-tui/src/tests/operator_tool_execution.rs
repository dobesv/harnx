use super::*;

#[tokio::test]
async fn call_bash_routes_raw_script_through_fenced_operator_command_path() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    let baseline = tui.app.transcript.len();
    // A missing script is a synchronous usage error: no background task, no
    // session requirement (#2630).
    tui.run_command(".call bash").await.unwrap();
    while let Ok(event) = tui.event_rx.try_recv() {
        tui.handle_tui_event(event).await.unwrap();
    }
    let usage = tui
        .app
        .transcript
        .iter()
        .skip(baseline)
        .filter_map(|entry| match entry {
            TranscriptItem::ErrorText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        usage.contains("Usage: .call bash <script>"),
        "missing script must render the usage error, got: {usage}"
    );
    // A real script routes through the fenced background operator command
    // task; without an active session its late result still renders the
    // clear session error, preserving the current task fencing.
    let baseline = tui.app.transcript.len();
    tui.run_command(".call bash echo \"spaced  out\" --flag")
        .await
        .unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), tui.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    tui.handle_tui_event(event).await.unwrap();
    let session_error = tui
        .app
        .transcript
        .iter()
        .skip(baseline)
        .filter_map(|entry| match entry {
            TranscriptItem::ErrorText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        session_error.contains("active session"),
        ".call bash without a session must report the missing session, got: {session_error}"
    );
}
