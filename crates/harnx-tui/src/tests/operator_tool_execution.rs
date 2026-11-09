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
    assert!(!tui.app.llm_busy);
    assert!(tui.current_prompt_abort.is_none());
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

/// #2655 regression: the raw script typed after `.call bash` must survive
/// command construction. The TUI dispatch chain (`input_commands.rs`) is
/// `is_operator_tool_command` → `parse_operator_line` →
/// `start_operator_tool_command`, which hands the parsed command to
/// `run_session_tool_command`; that builder copies it verbatim into
/// `OperatorToolRequest.command` before any network send. TUI tests cannot
/// install an active session without a NATS-backed worker, so this pins the
/// command-construction boundary itself: the script reaches the request
/// exactly as typed.
#[test]
fn call_bash_tui_command_construction_preserves_raw_script_tokens() {
    let line = ".call bash echo \"spaced  out\" --flag";
    assert!(
        harnx_runtime::operator_tools::is_operator_tool_command(line),
        "the TUI must dispatch {line} through the operator command path"
    );
    let command = harnx_runtime::operator_tools::parse_operator_line(line).unwrap();
    assert_eq!(
        command,
        harnx_runtime::operator_tools::OperatorToolCommand::Bash {
            script: "echo \"spaced  out\" --flag".into(),
            selection: harnx_runtime::operator_tools::BashToolSelection::Session,
        }
    );
    // The wire form built from that command keeps the script verbatim.
    let value = serde_json::to_value(&command).unwrap();
    assert_eq!(value["action"], "bash");
    assert_eq!(value["script"], "echo \"spaced  out\" --flag");
    assert_eq!(value["selection"]["mode"], "session");
}
