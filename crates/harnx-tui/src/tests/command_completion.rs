//! Tab-completion behaviour for the `.`-prefixed commands.

use super::*;

#[tokio::test]
async fn command_completions_separate_names_from_usage_and_offer_subcommands() {
    let config = test_config();
    let tui = Tui::init(&config).await.unwrap();

    for command in [".rewind", ".edit", ".delete", ".info"] {
        let completions = tui.compute_completions(command, command.len()).await;
        assert!(
            completions
                .iter()
                .all(|(value, _)| !["<n>", "[server]", "[name]", "<n>-<m>"]
                    .iter()
                    .any(|usage| value.contains(usage))),
            "{command} command completion contains a usage hint: {completions:?}"
        );
    }

    let edit_names = tui.compute_completions(".edit", ".edit".len()).await;
    assert_eq!(
        edit_names
            .iter()
            .filter(|(value, _)| value == ".edit message ")
            .count(),
        1,
        "duplicate .edit message completion: {edit_names:?}"
    );
    let delete_names = tui.compute_completions(".delete", ".delete".len()).await;
    assert_eq!(
        delete_names
            .iter()
            .filter(|(value, _)| value == ".delete message ")
            .count(),
        1,
        "duplicate .delete message completion: {delete_names:?}"
    );

    let mut edit_subcommands: Vec<String> = tui
        .compute_completions(".edit ", ".edit ".len())
        .await
        .into_iter()
        .map(|(value, _)| value)
        .collect();
    edit_subcommands.sort();
    assert_eq!(
        edit_subcommands,
        ["agent", "config", "message", "rag-docs", "session"]
    );

    let mut delete_subcommands: Vec<String> = tui
        .compute_completions(".delete ", ".delete ".len())
        .await
        .into_iter()
        .map(|(value, _)| value)
        .collect();
    delete_subcommands.sort();
    assert_eq!(
        delete_subcommands,
        ["agent", "agent-data", "macro", "message", "rag", "session"]
    );
}

#[tokio::test]
async fn removed_mcp_commands_are_not_completed() {
    let config = test_config();
    let tui = Tui::init(&config).await.unwrap();

    let mut info_subcommands: Vec<String> = tui
        .compute_completions(".info ", ".info ".len())
        .await
        .into_iter()
        .map(|(value, _)| value)
        .collect();
    info_subcommands.sort();

    assert_eq!(
        info_subcommands,
        [
            "agent",
            "env",
            "model",
            "rag",
            "session",
            "terminal_status",
            "theme",
            "tool",
            "tools"
        ]
    );
    assert!(
        tui.compute_completions(".mcp ", ".mcp ".len())
            .await
            .is_empty(),
        "removed .mcp commands must not offer subcommand completions"
    );
}

#[tokio::test]
async fn operator_tool_help_completion_and_missing_session_do_not_use_global_inventory() {
    let config = test_config();
    config.read().nats_tool_declarations.write().push(
        serde_json::from_value(serde_json::json!({
            "name":"hidden_global_tool", "description":"must not leak", "parameters":{}
        }))
        .unwrap(),
    );
    let mut tui = Tui::init(&config).await.unwrap();
    for (line, expected) in [(".call ", "tool"), (".list ", "tools")] {
        assert_eq!(
            tui.compute_completions(line, line.len())
                .await
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            [expected]
        );
    }
    for line in [".info tool ", ".list tools ", ".call tool "] {
        assert!(
            tui.compute_completions(line, line.len()).await.is_empty(),
            "{line}"
        );
    }
    let names = crate::completion::command_name_completions(".");
    for name in [".info tool ", ".list tools ", ".call tool "] {
        assert_eq!(names.iter().filter(|(value, _)| value == name).count(), 1);
    }
    tui.run_command(".help").await.unwrap();
    let help = tui
        .app
        .transcript
        .iter()
        .filter_map(|item| {
            if let TranscriptItem::SystemText(text) = item {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        help.contains(".info tool") && help.contains(".list tools") && help.contains(".call tool")
    );
    tui.run_command(".call tool visible []").await.unwrap();
    assert!(!tui.app.llm_busy);
    assert!(tui.current_prompt_handle.is_none());
    tui.run_command(".list tools").await.unwrap();
    assert!(
        tui.app.llm_busy,
        "operator request must run outside the event loop"
    );
    assert!(
        tui.active_remote_session.is_none(),
        "operator request must not be an inference turn"
    );
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), tui.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    tui.handle_tui_event(event).await.unwrap();
    assert!(!tui.app.llm_busy);
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::ErrorText(text) if text.contains("active session"))
    ));
    assert!(config.read().tui_confirm_tool_use.is_none());
}

#[tokio::test]
async fn operator_tool_completion_preserves_full_error_output_and_fences_cancelled_or_old_tasks() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    let task = harnx_runtime::utils::create_abort_signal();
    tui.current_prompt_abort = Some(task.clone());
    tui.app.llm_busy = true;
    let output = "{\n  \"structuredContent\": {\"kept\": true},\n  \"isError\": true\n}";
    tui.handle_tui_event(TuiEvent::OperatorToolFinished {
        task,
        output: output.into(),
        error: Some("tool failed".into()),
    })
    .await
    .unwrap();
    assert!(tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::SystemText(text) if text == output)));
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::ErrorText(text) if text.contains("tool failed"))
    ));
    assert!(!tui.app.llm_busy);
    let previous = harnx_runtime::utils::create_abort_signal();
    let cancelled = harnx_runtime::utils::create_abort_signal();
    cancelled.set_ctrlc();
    tui.current_prompt_abort = Some(cancelled.clone());
    tui.app.llm_busy = true;
    let before = tui.app.transcript.len();
    tui.handle_tui_event(TuiEvent::OperatorToolFinished {
        task: previous,
        output: "stale output".into(),
        error: None,
    })
    .await
    .unwrap();
    assert_eq!(tui.app.transcript.len(), before);
    assert!(tui.app.llm_busy);
    tui.handle_tui_event(TuiEvent::OperatorToolFinished {
        task: cancelled,
        output: "cancelled output".into(),
        error: None,
    })
    .await
    .unwrap();
    assert!(!tui.app.llm_busy);
    assert!(!tui.app.transcript.iter().any(|item| matches!(item, TranscriptItem::SystemText(text) if text.contains("cancelled output") || text.contains("stale output"))));
}
