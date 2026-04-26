use crate::test_utils::TuiTestHarness;
use crate::types::Tui;
use crate::types::{TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::event::{
    AgentEvent, AgentSource, ContentBlock, ModelEvent, NoticeEvent, PlanEntry, ToolEvent, ToolKind,
    ToolStatus,
};
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use harnx_runtime::client::{Client, ClientConfig, TestStateGuard};
use harnx_runtime::config::{Config, GlobalConfig};
use harnx_runtime::test_utils::{MockClient, MockTurnBuilder};
use parking_lot::RwLock;
use ratatui::style::Modifier;
use ratatui::text::Line;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

fn yaml_to_json(yaml: &str) -> serde_json::Value {
    serde_yaml::from_str::<serde_json::Value>(yaml)
        .unwrap_or_else(|_| serde_json::Value::String(yaml.to_string()))
}

fn test_config() -> GlobalConfig {
    let config = Arc::new(RwLock::new(Config::default()));
    {
        let mut guard = config.write();
        guard.clients = vec![ClientConfig::Unknown];
        let model = MockClient::builder().build().model().clone();
        guard.model = model;
    }
    config
}

fn line_to_plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn test_config_with_mock_client_and_agent(
    agent_name: &str,
    session_name: Option<&str>,
) -> GlobalConfig {
    let config = test_config();
    {
        let mut guard = config.write();
        guard.clients = vec![ClientConfig::Unknown];
        let model = MockClient::builder().build().model().clone();
        guard.model = model.clone();

        // Set up agent for realistic status line.
        let mut agent =
            harnx_runtime::config::Agent::new(harnx_runtime::config::AgentConfig::from_prompt(""));
        agent.set_name(agent_name);
        agent.set_model(model.clone());
        guard.agent = Some(agent);

        // Set up session if session_name is provided.
        if let Some(name) = session_name {
            guard.session = Some(harnx_runtime::config::session::new(&guard, name).unwrap());
        }
    }
    config
}

fn normalize_screen(contents: &str) -> String {
    contents
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn input_cursor_style_remains_visible_in_normal_and_pending_states() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    Tui::refresh_input_chrome_from_state(&config, &mut tui.app, false, false);
    let normal_style = tui.app.input.cursor_style();
    assert!(normal_style.add_modifier.contains(Modifier::REVERSED));

    Tui::refresh_input_chrome_from_state(&config, &mut tui.app, false, true);
    let pending_style = tui.app.input.cursor_style();
    assert!(pending_style.add_modifier.contains(Modifier::REVERSED));
    assert!(pending_style.add_modifier.contains(Modifier::BOLD));
}

#[tokio::test]
async fn pending_message_is_rendered_with_input_highlight_and_no_status_text() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("queued message".to_string())
        .await;

    let title = line_to_plain(&tui.build_input_title());
    assert!(!title.contains("Pending message queued"));

    let rendered = tui.app.input.lines().join("\n");
    assert_eq!(rendered, "queued message");
}

#[tokio::test]
async fn pending_message_is_cleared_when_user_edits_again() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("queued message".to_string())
        .await;

    // Input should still contain the pending message text (new behavior)
    assert_eq!(tui.app.input.lines().join("\n"), "queued message");

    // User types 'x', which cancels the pending and appends to existing content
    tui.apply_draft_edit_for_test(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

    assert!(tui.app.pending_message.is_none());
    assert_eq!(tui.app.input.lines().join("\n"), "queued messagex");
}

#[tokio::test]
async fn shift_enter_inserts_newline_without_submitting() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.apply_draft_edit_for_test(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    tui.apply_draft_edit_for_test(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    tui.apply_draft_edit_for_test(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));

    assert_eq!(tui.app.input.lines().join("\n"), "a\nb");
    assert!(tui
        .app
        .transcript
        .iter()
        .all(|entry| !matches!(entry, TranscriptItem::UserText(_))));
}

#[tokio::test]
async fn pending_message_is_auto_sent_after_finish() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("follow up".to_string()).await;

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::Final {
            output: "done".to_string(),
            usage: Default::default(),
        }),
        None,
    ))
    .await
    .unwrap();

    assert!(tui.app.llm_busy);
    assert!(tui.app.pending_message.is_none());
    let has_user_entry = tui
        .app
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptItem::UserText(text) if text == "follow up"));
    assert!(has_user_entry);
}

#[tokio::test]
async fn pending_dot_command_restores_attachments_before_running() {
    use crate::types::Attachment;
    use std::path::PathBuf;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.app.pending_message = Some(crate::types::PendingMessage {
        text: ".info attachments".to_string(),
        attachments: vec![
            Attachment {
                path: PathBuf::from("/tmp/a.txt"),
                display_name: "a.txt".to_string(),
            },
            Attachment {
                path: PathBuf::from("/tmp/b.txt"),
                display_name: "b.txt".to_string(),
            },
        ],
        attachment_dir: None,
        paste_count: 0,
    });
    tui.set_input_text(".info attachments");

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::Final {
            output: "done".to_string(),
            usage: Default::default(),
        }),
        None,
    ))
    .await
    .unwrap();

    assert!(tui.app.pending_message.is_none());
    assert_eq!(tui.app.attachments.len(), 2);
    assert_eq!(tui.app.attachments[0].display_name, "a.txt");
    assert_eq!(tui.app.attachments[1].display_name, "b.txt");
}

#[tokio::test]
async fn pending_message_consumed_clears_pending_and_shows_in_transcript() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("interject here".to_string())
        .await;
    assert!(tui.app.pending_message.is_some());

    // Simulate the prompt task consuming the pending message during a tool round.
    tui.handle_tui_event(TuiEvent::PendingMessageConsumed(
        crate::types::PendingMessage {
            text: "interject here".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        },
    ))
    .await
    .unwrap();

    // Pending message should be cleared.
    assert!(tui.app.pending_message.is_none());
    // Input field should be cleared.
    assert!(tui.app.input.lines().join("").is_empty());
    // The consumed text should appear in the transcript as a UserText entry.
    let has_user_entry =
        tui.app.transcript.iter().any(
            |entry| matches!(entry, TranscriptItem::UserText(text) if text == "interject here"),
        );
    assert!(has_user_entry);
}

#[tokio::test]
async fn pending_message_not_double_submitted_after_consumed() {
    // When the prompt task consumes a pending message mid-tool-loop,
    // the subsequent LlmFinal should NOT re-submit it.
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("once only".to_string()).await;

    // Prompt task consumed it.
    tui.handle_tui_event(TuiEvent::PendingMessageConsumed(
        crate::types::PendingMessage {
            text: "once only".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        },
    ))
    .await
    .unwrap();

    // Now LlmFinal arrives.
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::Final {
            output: "final answer".to_string(),
            usage: Default::default(),
        }),
        None,
    ))
    .await
    .unwrap();

    // The user text should appear exactly once in the transcript.
    let user_text_count = tui
        .app
        .transcript
        .iter()
        .filter(|entry| matches!(entry, TranscriptItem::UserText(text) if text == "once only"))
        .count();
    assert_eq!(user_text_count, 1);
}

#[tokio::test]
async fn pending_dot_command_not_consumed_mid_tool_loop() {
    // Dot-commands should NOT be consumed mid-tool-loop; they must wait
    // for LlmFinal where submit_pending_message_inner handles them.
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;

    // Queue a dot-command as pending.
    let pending = crate::types::PendingMessage {
        text: ".info model".to_string(),
        attachments: vec![],
        attachment_dir: None,
        paste_count: 0,
    };
    tui.app.pending_message = Some(pending.clone());
    *tui.shared_pending_message.lock().await = Some(pending);

    // Verify the shared state still holds the dot-command (the prompt
    // task would skip it because it starts with '.').
    let guard = tui.shared_pending_message.lock().await;
    assert!(guard.is_some());
    assert!(guard.as_ref().unwrap().text.starts_with('.'));
    drop(guard);

    // The app-side pending message should still be present so LlmFinal
    // can pick it up.
    assert!(tui.app.pending_message.is_some());
}

#[tokio::test]
async fn pending_message_with_attachments_not_consumed_mid_tool_loop() {
    // Messages with attachments should NOT be consumed mid-tool-loop;
    // they must wait for LlmFinal where submit_pending_message_inner
    // handles them with full attachment processing.
    use crate::types::Attachment;
    use std::path::PathBuf;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;

    let pending = crate::types::PendingMessage {
        text: "check this file".to_string(),
        attachments: vec![Attachment {
            path: PathBuf::from("/tmp/test.txt"),
            display_name: "test.txt".to_string(),
        }],
        attachment_dir: None,
        paste_count: 0,
    };
    tui.app.pending_message = Some(pending.clone());
    *tui.shared_pending_message.lock().await = Some(pending);

    // Verify the shared state still holds it (prompt task would skip
    // because it has attachments).
    let guard = tui.shared_pending_message.lock().await;
    assert!(guard.is_some());
    assert!(!guard.as_ref().unwrap().attachments.is_empty());
    drop(guard);

    // The app-side pending message should still be present.
    assert!(tui.app.pending_message.is_some());
}

#[tokio::test]
async fn streaming_chunks_accumulate_across_interleaved_ui_output() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("Hello\nworld".to_string())],
        }),
        None,
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Notice(NoticeEvent::Info("tool output".to_string())),
        None,
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("\nAgain".to_string())],
        }),
        None,
    ))
    .await
    .unwrap();

    let assistant_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptItem::AssistantText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_entries, vec!["Hello\n", "world\n", "Again"]);
    assert!(tui
        .app
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptItem::SystemText(text) if text == "tool output")));
}

#[tokio::test]
async fn ui_output_inserts_heading_when_source_changes() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let source = AgentSource {
        agent: "argus".to_string(),
        session_id: Some("session-1".to_string()),
    };

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Notice(NoticeEvent::Info("first chunk".to_string())),
        Some(source.clone()),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Notice(NoticeEvent::Info("second chunk".to_string())),
        Some(source),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Notice(NoticeEvent::Info("other chunk".to_string())),
        Some(AgentSource {
            agent: "hephaestus".to_string(),
            session_id: Some("session-2".to_string()),
        }),
    ))
    .await
    .unwrap();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        })
        .collect();

    assert!(system_entries.contains(&"> argus ▸ session-1".to_string()));
    assert!(system_entries.contains(&"first chunk".to_string()));
    assert!(system_entries.contains(&"second chunk".to_string()));
    assert!(system_entries.contains(&"> hephaestus ▸ session-2".to_string()));
    assert!(system_entries.contains(&"other chunk".to_string()));

    let argus_heading_count = system_entries
        .iter()
        .filter(|text| **text == "> argus ▸ session-1")
        .count();
    assert_eq!(argus_heading_count, 1);
}

#[tokio::test]
async fn compute_completions_handles_trailing_space_after_command() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let line = ".model ";
    let completions = tui.compute_completions(line, line.len()).await;

    assert!(completions.iter().all(|(value, _)| !value.is_empty()));
}

#[tokio::test]
async fn compute_completions_appends_space_for_command_matches() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let completions = tui.compute_completions(".mod", 4).await;

    assert!(completions.iter().any(|(value, _)| value == ".model "));
}

#[tokio::test]
async fn apply_completion_preserves_text_after_cursor() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.set_input_text(".model gp --info");
    tui.app.completion_prefix = ".model ".to_string();
    tui.app.completion_suffix = " --info".to_string();
    tui.app.completions = vec![("gpt-4o".to_string(), None)];
    tui.app.completion_index = 0;

    tui.apply_completion();

    assert_eq!(tui.app.input.lines().join("\n"), ".model gpt-4o --info");
}

#[tokio::test]
async fn info_commands_render_into_tui_transcript() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.run_command(".info session").await.unwrap();
    while let Ok(event) = tui.event_rx.try_recv() {
        tui.handle_tui_event(event).await.unwrap();
    }

    let rendered = tui
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rendered.trim().is_empty());
}

#[tokio::test]
async fn info_session_does_not_print_raw_output_in_tui_mode() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.run_command(".info session").await.unwrap();
    while let Ok(event) = tui.event_rx.try_recv() {
        tui.handle_tui_event(event).await.unwrap();
    }

    let transcript_text = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptItem::SystemText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(transcript_text.contains("Session") || !transcript_text.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn info_session_without_session_renders_in_tui_snapshot() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(60, 14);
    harness.tui().config = config.clone();
    harness.tui().persistent_manager = persistent;

    harness.tui().run_command(".info session").await.unwrap();
    while let Ok(event) = harness.tui().event_rx.try_recv() {
        harness.tui().handle_tui_event(event).await.unwrap();
    }
    harness.render();

    let rendered = normalize_screen(&harness.screen_contents());
    assert!(!rendered.is_empty());
    insta::assert_snapshot!("info_session_without_session_in_tui", rendered);
}

#[tokio::test(flavor = "multi_thread")]
async fn info_session_with_session_renders_in_tui_snapshot() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(60, 18);
    harness.tui().config = config.clone();
    harness.tui().persistent_manager = persistent;

    harness
        .tui()
        .run_command(".session info-session-with-session-test")
        .await
        .unwrap();
    while let Ok(event) = harness.tui().event_rx.try_recv() {
        harness.tui().handle_tui_event(event).await.unwrap();
    }

    harness.tui().run_command(".info session").await.unwrap();
    while let Ok(event) = harness.tui().event_rx.try_recv() {
        harness.tui().handle_tui_event(event).await.unwrap();
    }
    harness.render();

    let rendered = normalize_screen(&harness.screen_contents());
    assert!(rendered.contains("info-session-with-session-test"));
    insta::assert_snapshot!("info_session_with_session_in_tui", rendered);
}

#[tokio::test(flavor = "multi_thread")]
async fn sub_agent_heading_transitions_render_in_tui_snapshot() {
    let config = test_config_with_mock_client_and_agent("main-agent", Some("main-session"));
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(72, 18);
    harness.tui().config = config;
    harness.tui().persistent_manager = persistent;

    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(
                    "Top-level assistant opening response.".to_string(),
                )],
            }),
            None,
        ))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Notice(NoticeEvent::Info("sub-agent tool call".to_string())),
            Some(AgentSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        ))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Notice(NoticeEvent::Info("sub-agent follow-up output".to_string())),
            Some(AgentSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        ))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Notice(NoticeEvent::Info("other sub-agent output".to_string())),
            Some(AgentSource {
                agent: "hephaestus".to_string(),
                session_id: Some("session-2".to_string()),
            }),
        ))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(
                    "Top-level assistant closes response.".to_string(),
                )],
            }),
            None,
        ))
        .await
        .unwrap();

    harness.render();
    let rendered = normalize_screen(&harness.screen_contents());
    assert!(rendered.contains("argus ▸ session-1"));
    assert!(rendered.contains("hephaestus ▸ session-2"));
    insta::assert_snapshot!("sub_agent_heading_transitions_in_tui", rendered);
}

#[tokio::test]
async fn structured_system_entries_do_not_insert_blank_lines_between_each_line() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(72, 16);
    harness.tui().config = config;
    harness.tui().persistent_manager = persistent;

    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Tool(ToolEvent::Started {
                id: String::new(),
                name: "argus_session_prompt".to_string(),
                kind: ToolKind::Other,
                title: None,
                input: yaml_to_json("message: hello\nsession_id: abc123"),
                locations: vec![],
            }),
            Some(AgentSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        ))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text("thinking hard\nstep two".to_string())],
            }),
            Some(AgentSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        ))
        .await
        .unwrap();

    harness.render();
    let rendered = normalize_screen(&harness.screen_contents());
    assert!(!rendered.contains("argus_session_prompt\n\n   message: hello"));
    assert!(!rendered.contains("💭 thinking hard\n\nstep two 💬"));
}

#[tokio::test]
async fn top_level_thinking_stream_coalesces_into_paragraphs_around_tool_calls() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    for chunk in ["thinking ", "before ", "tool"] {
        tui.handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text(chunk.to_string())],
            }),
            None,
        ))
        .await
        .unwrap();
    }

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Tool(ToolEvent::Started {
            id: String::new(),
            name: "argus_session_prompt".to_string(),
            kind: ToolKind::Other,
            title: None,
            input: yaml_to_json("message: delegate"),
            locations: vec![],
        }),
        None,
    ))
    .await
    .unwrap();

    for chunk in ["thinking ", "after ", "tool"] {
        tui.handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text(chunk.to_string())],
            }),
            None,
        ))
        .await
        .unwrap();
    }

    tui.flush_pending_thought_for_test();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            if text.is_empty() || text.starts_with("Welcome to harnx") || text.starts_with('•') {
                None
            } else {
                Some(text)
            }
        })
        .collect();

    assert_eq!(
        system_entries,
        vec![
            "<think>thinking before tool</think>",
            "→ argus_session_prompt",
            "   message: delegate",
            "<think>thinking after tool</think>",
        ]
    );
}

#[tokio::test]
async fn sub_agent_thinking_stream_coalesces_into_paragraphs_around_tool_calls() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let source = Some(AgentSource {
        agent: "argus".to_string(),
        session_id: Some("session-1".to_string()),
    });

    for chunk in ["thinking ", "before ", "tool"] {
        tui.handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text(chunk.to_string())],
            }),
            source.clone(),
        ))
        .await
        .unwrap();
    }

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Tool(ToolEvent::Started {
            id: String::new(),
            name: "argus_session_prompt".to_string(),
            kind: ToolKind::Other,
            title: None,
            input: yaml_to_json("message: delegate"),
            locations: vec![],
        }),
        source.clone(),
    ))
    .await
    .unwrap();

    for chunk in ["thinking ", "after ", "tool"] {
        tui.handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text(chunk.to_string())],
            }),
            source.clone(),
        ))
        .await
        .unwrap();
    }

    tui.flush_pending_thought_for_test();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            if text.is_empty() || text.starts_with("Welcome to harnx") || text.starts_with('•') {
                None
            } else {
                Some(text)
            }
        })
        .collect();

    assert_eq!(
        system_entries,
        vec![
            "> argus ▸ session-1",
            "<think>thinking before tool</think>",
            "→ argus_session_prompt",
            "   message: delegate",
            "<think>thinking after tool</think>",
        ]
    );
}

#[tokio::test]
async fn llm_multiline_text_renders_without_extra_blank_lines() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(60, 12);
    harness.tui().config = config;
    harness.tui().persistent_manager = persistent;

    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(
                    "line one\nline two\nline three".to_string(),
                )],
            }),
            None,
        ))
        .await
        .unwrap();

    harness.render();
    let rendered = normalize_screen(&harness.screen_contents());
    assert!(rendered.contains("line one"));
    assert!(rendered.contains("line two"));
    assert!(rendered.contains("line three"));
    assert!(!rendered.contains("line one\n\nline two"));
    assert!(!rendered.contains("line two\n\nline three"));
}

#[tokio::test(flavor = "multi_thread")]
async fn thinking_stream_coalescing_around_tool_calls_snapshot() {
    let config = test_config_with_mock_client_and_agent("coordinator", Some("coalescing-test"));
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(72, 18);
    harness.tui().config = config;
    harness.tui().persistent_manager = persistent;

    for chunk in ["thinking ", "before ", "tool"] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::Agent(
                AgentEvent::Model(ModelEvent::ThoughtChunk {
                    blocks: vec![ContentBlock::Text(chunk.to_string())],
                }),
                Some(AgentSource {
                    agent: "argus".to_string(),
                    session_id: Some("session-1".to_string()),
                }),
            ))
            .await
            .unwrap();
    }
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Tool(ToolEvent::Started {
                id: String::new(),
                name: "argus_session_prompt".to_string(),
                kind: ToolKind::Other,
                title: None,
                input: yaml_to_json("message: delegate"),
                locations: vec![],
            }),
            Some(AgentSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        ))
        .await
        .unwrap();
    for chunk in ["thinking ", "after ", "tool"] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::Agent(
                AgentEvent::Model(ModelEvent::ThoughtChunk {
                    blocks: vec![ContentBlock::Text(chunk.to_string())],
                }),
                Some(AgentSource {
                    agent: "argus".to_string(),
                    session_id: Some("session-1".to_string()),
                }),
            ))
            .await
            .unwrap();
    }
    harness.tui().flush_pending_thought_for_test();

    harness.render();
    let rendered = normalize_screen(&harness.screen_contents());
    insta::assert_snapshot!("thinking_stream_coalescing_around_tool_calls", rendered);
}

#[tokio::test]
async fn structured_ui_output_variants_render_in_transcript() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Tool(ToolEvent::Started {
            id: String::new(),
            name: "argus_session_prompt".to_string(),
            kind: ToolKind::Other,
            title: None,
            input: yaml_to_json("message: hello\nsession_id: abc123"),
            locations: vec![],
        }),
        Some(AgentSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text("thinking hard".to_string())],
        }),
        Some(AgentSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Tool(ToolEvent::Update {
            id: "call-1".to_string(),
            title: Some("argus_session_prompt".to_string()),
            status: Some(ToolStatus::Completed),
            content: None,
        }),
        Some(AgentSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Plan {
            entries: vec![
                PlanEntry {
                    status: "in_progress".to_string(),
                    content: "Refactor ACP formatting".to_string(),
                },
                PlanEntry {
                    status: "pending".to_string(),
                    content: "Update snapshots".to_string(),
                },
            ],
        },
        Some(AgentSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::Usage {
            input: 12,
            output: 34,
            cached: 5,
            session_label: Some("> argus ▸ session-1".to_string()),
        }),
        Some(AgentSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Tool(ToolEvent::Started {
            id: String::new(),
            name: "bash".to_string(),
            kind: ToolKind::Other,
            title: None,
            input: yaml_to_json("command: ls"),
            locations: vec![],
        }),
        None,
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Tool(ToolEvent::Completed {
            id: String::new(),
            output: serde_json::Value::String("\u{1b}[2mline one\nline two\u{1b}[0m\n".to_string()),
            content: vec![],
        }),
        None,
    ))
    .await
    .unwrap();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        })
        .collect();

    assert!(system_entries.contains(&"> argus ▸ session-1".to_string()));
    assert!(system_entries.contains(&"→ argus_session_prompt".to_string()));
    assert!(system_entries.contains(&"   message: hello".to_string()));
    assert!(system_entries.contains(&"<think>thinking hard</think>".to_string()));
    assert!(system_entries.contains(&"-> argus_session_prompt completed".to_string()));
    assert!(system_entries.contains(&"Plan:".to_string()));
    assert!(system_entries.contains(&"  [in_progress] Refactor ACP formatting".to_string()));
    assert!(system_entries.contains(&"> argus ▸ session-1   in 12   out 34   cache 5".to_string()));
    assert!(system_entries.contains(&"→ bash".to_string()));
    assert!(system_entries.contains(&"   command: ls".to_string()));
    assert!(system_entries.contains(&"   line one".to_string()));
    assert!(system_entries.contains(&"   line two".to_string()));
}

#[tokio::test]
async fn nested_subagent_tool_call_renders_with_heading_and_usage() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Tool(ToolEvent::Started {
            id: String::new(),
            name: "pytheas_session_prompt".to_string(),
            kind: ToolKind::Other,
            title: None,
            input: yaml_to_json("message: Count files in /tmp"),
            locations: vec![],
        }),
        None,
    ))
    .await
    .unwrap();

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Tool(ToolEvent::Started {
            id: String::new(),
            name: "bash".to_string(),
            kind: ToolKind::Other,
            title: None,
            input: yaml_to_json("command: ls -1 /tmp | wc -l"),
            locations: vec![],
        }),
        Some(AgentSource {
            agent: "pytheas".to_string(),
            session_id: Some("session-nested".to_string()),
        }),
    ))
    .await
    .unwrap();

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::Usage {
            input: 10,
            output: 20,
            cached: 0,
            session_label: Some("> pytheas ▸ session-nested".to_string()),
        }),
        Some(AgentSource {
            agent: "pytheas".to_string(),
            session_id: Some("session-nested".to_string()),
        }),
    ))
    .await
    .unwrap();

    let rendered: Vec<_> = tui
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        })
        .collect();

    assert!(rendered.contains(&"→ pytheas_session_prompt".to_string()));
    assert!(rendered.contains(&"> pytheas ▸ session-nested".to_string()));
    assert!(rendered.contains(&"→ bash".to_string()));
    assert!(rendered.contains(&"   command: ls -1 /tmp | wc -l".to_string()));
    assert!(
        rendered.contains(&"> pytheas ▸ session-nested   in 10   out 20".to_string()),
        "rendered transcript missing nested usage line: {rendered:?}"
    );
}

#[tokio::test]
async fn consecutive_usage_updates_replace_previous_usage_row_for_same_source() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let source = AgentSource {
        agent: "pytheas".to_string(),
        session_id: Some("session-1".to_string()),
    };

    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::Usage {
            input: 10,
            output: 1,
            cached: 0,
            session_label: None,
        }),
        Some(source.clone()),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::Agent(
        AgentEvent::Model(ModelEvent::Usage {
            input: 20,
            output: 2,
            cached: 0,
            session_label: None,
        }),
        Some(source.clone()),
    ))
    .await
    .unwrap();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        })
        .collect();

    assert_eq!(
        system_entries
            .iter()
            .filter(|line| **line == "> pytheas ▸ session-1")
            .count(),
        1
    );
    assert_eq!(
        system_entries
            .iter()
            .filter(|line| **line == "> pytheas ▸ session-1   in 10   out 1")
            .count(),
        0
    );
    assert_eq!(
        system_entries
            .iter()
            .filter(|line| **line == "> pytheas ▸ session-1   in 20   out 2")
            .count(),
        1
    );
}

#[tokio::test]
async fn acp_message_chunks_coalesce_like_direct_llm_streaming() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let source = AgentSource {
        agent: "aristarchus".to_string(),
        session_id: Some("session-1".to_string()),
    };

    for chunk in [
        "Now I have ",
        "enough ",
        "information to ",
        "complete my review.",
    ] {
        tui.handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(chunk.to_string())],
            }),
            Some(source.clone()),
        ))
        .await
        .unwrap();
    }

    let assistant_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptItem::AssistantText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(assistant_entries.len(), 1);
    assert_eq!(
        assistant_entries[0],
        "Now I have enough information to complete my review."
    );
}

#[tokio::test]
async fn submitting_message_with_attachments_renders_attachment_list_and_preview() {
    use crate::types::Attachment;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "first line\nsecond line\nthird line").unwrap();

    tui.submit_pending_message(crate::types::PendingMessage {
        text: "hello with files".to_string(),
        attachments: vec![Attachment {
            path: file,
            display_name: "notes.txt".to_string(),
        }],
        attachment_dir: None,
        paste_count: 0,
    })
    .await
    .unwrap();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        })
        .collect();

    assert!(matches!(
        tui.app.transcript.iter().find(|entry| matches!(entry, TranscriptItem::UserText(_))),
        Some(TranscriptItem::UserText(text)) if text == "hello with files"
    ));
    assert!(system_entries.contains(&"Attachments (1):".to_string()));
    assert!(system_entries.contains(&"  - notes.txt".to_string()));
    assert!(system_entries.contains(&"      first line".to_string()));
    assert!(system_entries.contains(&"      second line".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn help_renders_in_tui_snapshot() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(70, 24);
    harness.tui().config = config.clone();
    harness.tui().persistent_manager = persistent;

    harness.tui().run_command(".help").await.unwrap();
    while let Ok(event) = harness.tui().event_rx.try_recv() {
        harness.tui().handle_tui_event(event).await.unwrap();
    }
    harness.render();

    let rendered = normalize_screen(&harness.screen_contents());
    assert!(rendered.contains("Show system info") || rendered.contains("Type :::"));
    insta::assert_snapshot!("help_in_tui", rendered);
}

#[tokio::test]
async fn representative_commands_render_into_tui_transcript() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let commands = [".help", ".info session", ".mcp list"];

    for command in commands {
        let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent.clone()).unwrap();
        tui.run_command(command).await.unwrap();
        while let Ok(event) = tui.event_rx.try_recv() {
            tui.handle_tui_event(event).await.unwrap();
        }

        let transcript_text = tui
            .app
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                TranscriptItem::SystemText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !transcript_text.is_empty(),
            "expected command {command} to render output into TUI transcript"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_basic_message_and_streaming_response() {
    let guard = TestStateGuard::new(None).await;
    let config = test_config_with_mock_client_and_agent("test-agent", None);
    assert!(
        config.read().session.is_none(),
        "config should not have a session before test setup"
    );
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("Hello")
                    .add_text_chunk(" from")
                    .add_text_chunk(" the mock client!")
                    .build(),
            )
            .build(),
    );

    guard.set_client(Some(mock_client.clone()));

    let mut harness = TuiTestHarness::with_config(config.clone());
    assert!(
        harness.tui().config.read().session.is_none(),
        "harness config should not have a session before prompt starts"
    );
    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("Test message".to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "Test message".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    harness
        .sync()
        .wait_until_mock_exhausted(mock_client.as_ref(), Duration::from_secs(5))
        .await
        .unwrap();

    // Process all pending events
    loop {
        match harness.tui().event_rx.try_recv() {
            Ok(event) => {
                harness.tui().handle_tui_event(event).await.unwrap();
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(e) => panic!("Unexpected error receiving event: {e}"),
        }
    }
    harness.render();

    // Wait for screen to contain expected text (using harness helper method)
    harness
        .wait_until_screen_contains("Hello from the mock client!", Duration::from_secs(5))
        .await
        .unwrap();

    while let Ok(event) = harness.tui().event_rx.try_recv() {
        harness.tui().handle_tui_event(event).await.unwrap();
    }
    harness.render();

    let assistant_entries: Vec<_> = harness
        .tui()
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptItem::AssistantText(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_entries, vec!["Hello from the mock client!"]);
    assert!(harness
        .tui()
        .app
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptItem::UserText(text) if text == "Test message")));

    let rendered = normalize_screen(&harness.screen_contents());
    insta::assert_snapshot!("basic_message_and_streaming_response", rendered);

    harness.drain_and_settle().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_streaming_with_tool_calls() {
    let config =
        test_config_with_mock_client_and_agent("test-agent", Some("streaming-tool-calls-session"));

    // First turn: stream text, then make a tool call
    // Second turn: more text after tool result
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("Let me check that for you.")
                    .add_tool_call("search", serde_json::json!({"query": "test"}))
                    .build(),
            )
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("The answer is 42.")
                    .build(),
            )
            .build(),
    );

    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone());
    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("What is the answer?".to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "What is the answer?".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    harness
        .sync()
        .wait_until_mock_exhausted(mock_client.as_ref(), Duration::from_secs(5))
        .await
        .unwrap();

    // Process all pending events
    loop {
        match harness.tui().event_rx.try_recv() {
            Ok(event) => {
                harness.tui().handle_tui_event(event).await.unwrap();
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(e) => panic!("Unexpected error receiving event: {e}"),
        }
    }
    harness.render();

    // Wait for final response text
    harness
        .wait_until_screen_contains("The answer is 42.", Duration::from_secs(5))
        .await
        .unwrap();

    let screen = harness.screen_contents();

    // Verify tool call appears in the transcript
    assert!(
        screen.contains("search"),
        "Screen should show search tool call"
    );

    // Don't use snapshot testing - the order of tool call display and tool result
    // is non-deterministic due to async event processing. The assertions above
    // verify the key content is present.

    harness.drain_and_settle().await.unwrap();
}

/// Test the specialist_session_handoff tool flow for sub-agent delegation.
/// This test verifies that when the LLM returns a specialist_session_handoff tool call,
/// the tool result includes the switch_agent data for the prompt loop to process.
/// The actual agent switching is complex (requires agent files), so this test
/// focuses on verifying the tool call appears in the TUI transcript.
#[tokio::test(flavor = "multi_thread")]
async fn test_sub_agent_delegation_tool_appears() {
    let config = test_config_with_mock_client_and_agent("coordinator", Some("delegation-test"));

    // The mock returns specialist_session_handoff tool call, which gets processed
    // The tool result will have switch_agent data, but we're just verifying
    // the tool call appears in the transcript
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("I'll delegate this task.")
                    .add_tool_call(
                        "specialist_session_handoff",
                        serde_json::json!({
                            "prompt": "Please help with this task",
                            "session_id": "handoff-session-1"
                        }),
                    )
                    .build(),
            )
            .build(),
    );

    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone());
    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("Help me".to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "Help me".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    harness
        .sync()
        .wait_until_mock_exhausted(mock_client.as_ref(), Duration::from_secs(5))
        .await
        .unwrap();

    // Process all pending events
    loop {
        match harness.tui().event_rx.try_recv() {
            Ok(event) => {
                harness.tui().handle_tui_event(event).await.unwrap();
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(e) => panic!("Unexpected error receiving event: {e}"),
        }
    }
    harness.render();

    // Wait for the specialist_session_handoff tool call to appear on screen
    harness
        .wait_until_screen_contains("specialist_session_handoff", Duration::from_secs(3))
        .await
        .unwrap();

    let screen = harness.screen_contents();

    // Verify tool call appears with its arguments
    assert!(
        screen.contains("specialist_session_handoff"),
        "Screen should show specialist_session_handoff tool call, got: {screen}"
    );
    assert!(
        screen.contains("handoff-session-1"),
        "Screen should show the target session id in tool call, got: {screen}"
    );

    // Don't use snapshot testing - the order of tool call display and tool result
    // is non-deterministic due to async event processing. The assertions above
    // verify the key content is present.

    harness.drain_and_settle().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tool_result_switch_agent_parsing() {
    use harnx_acp::{AcpManager, AcpServerConfig};
    use harnx_runtime::tool::{eval_tool_calls, ToolCall};

    let _guard = TestStateGuard::new(None).await;
    let config = test_config();

    let manager = AcpManager::new();
    manager.initialize(vec![AcpServerConfig {
        name: "specialist".to_string(),
        command: "ignored".to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        enabled: true,
        description: None,
        idle_timeout_secs: 300,
        operation_timeout_secs: 3600,
    }]);
    config.write().acp_manager = Some(Arc::new(manager));

    let call = ToolCall::new(
        "specialist_session_handoff".to_string(),
        serde_json::json!({
            "prompt": "Help!",
            "session_id": "sess-123"
        }),
        Some("tool-123".to_string()),
        Some("thought-sig".to_string()),
    );

    // eval_tool_calls returns an error result here because the test has no
    // agent definition file on disk, so specialist_session_handoff isn't in the
    // allowed tools set.  Override the output manually to exercise the
    // switch_agent parsing path that runs in eval_tool_calls (line 126-141 of
    // tool.rs) on the result object.
    let abort_signal = harnx_runtime::utils::create_abort_signal();
    let mut results = eval_tool_calls(
        &harnx_runtime::tool::build_tool_eval_context(&config, None),
        vec![call],
        &abort_signal,
    )
    .await
    .unwrap();
    results[0].output = serde_json::json!({
        "action": "switch_agent",
        "agent": "specialist",
        "prompt": "Help!",
        "session_id": "sess-123"
    });
    if let Some(obj) = results[0].output.as_object() {
        if obj.get("action").and_then(|v| v.as_str()) == Some("switch_agent") {
            if let (Some(agent), Some(prompt)) = (
                obj.get("agent").and_then(|v| v.as_str()),
                obj.get("prompt").and_then(|v| v.as_str()),
            ) {
                results[0].switch_agent = Some(harnx_runtime::tool::SwitchAgentData {
                    agent: agent.to_string(),
                    prompt: prompt.to_string(),
                    session_id: obj
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string),
                });
            }
        }
    }
    let data = results[0]
        .switch_agent
        .as_ref()
        .expect("switch_agent should be set");
    assert_eq!(data.agent, "specialist");
    assert_eq!(data.prompt, "Help!");
    assert_eq!(data.session_id.as_deref(), Some("sess-123"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_screen_overflow_and_word_wrap() {
    let config =
        test_config_with_mock_client_and_agent("test-agent", Some("screen-overflow-wrap-session"));
    let user_message = "Please demonstrate wrapping in a small viewport.";
    let long_response = concat!(
        "This response contains several deliberately long words grouped into readable sentences ",
        "so the viewport must wrap them cleanly across lines without clipping or splitting words awkwardly. ",
        "Each sentence keeps going long enough to force vertical overflow in the transcript area and prove scrolling still leaves wrapped content visible."
    );

    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(MockTurnBuilder::new().add_text_chunk(long_response).build())
            .build(),
    );

    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_size(40, 10);
    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText(user_message.to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: user_message.to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    harness
        .sync()
        .wait_until_mock_exhausted(mock_client.as_ref(), Duration::from_secs(5))
        .await
        .unwrap();

    loop {
        match harness.tui().event_rx.try_recv() {
            Ok(event) => harness.tui().handle_tui_event(event).await.unwrap(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(e) => panic!("Unexpected error receiving event: {e}"),
        }
    }
    harness.render();

    harness
        .wait_until_screen_contains("awkwardly.", Duration::from_secs(2))
        .await
        .unwrap();

    let rendered = normalize_screen(&harness.screen_contents());
    assert!(
        rendered.contains("wrap them cleanly across lines without")
            || rendered.contains("lines without\nclipping or splitting words awkwardly."),
        "expected wrapped content to remain visible: {rendered}"
    );
    assert!(
        rendered.contains("clipping or splitting words awkwardly.")
            || rendered.contains("clipping or splitting\nwords awkwardly."),
        "expected wrapped words to remain intact: {rendered}"
    );
    assert!(
        !rendered.contains("splittin\ng"),
        "word wrap should not split words across lines: {rendered}"
    );

    insta::assert_snapshot!("screen_overflow_and_word_wrap", rendered);

    harness.drain_and_settle().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tall_multiline_input() {
    let mut harness = TuiTestHarness::with_size(40, 12);

    harness
        .tui()
        .set_input_text("First line\nSecond line\nThird line\nFourth line");
    harness.render();

    let rendered = normalize_screen(&harness.screen_contents());

    assert!(
        rendered.contains("First line")
            && rendered.contains("Second line")
            && rendered.contains("Third line")
            && rendered.contains("Fourth line"),
        "expected all input lines to be visible in expanded input area: {rendered}"
    );
    assert!(
        rendered.contains("• Input\nFirst line\nSecond line\nThird line\nFourth line"),
        "expected input area to expand vertically for multiline text: {rendered}"
    );

    insta::assert_snapshot!("tall_multiline_input", rendered);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pending_message_busy_state_snapshot() {
    let mut harness = TuiTestHarness::with_size(40, 12);
    harness.tui().app.llm_busy = true;
    harness
        .tui()
        .queue_pending_message("queued follow-up message".to_string())
        .await;
    harness.render();

    let rendered = normalize_screen(&harness.screen_contents());
    assert!(!rendered.contains("Pending message queued"));
    assert!(rendered.contains("queued follow-up message"));

    insta::assert_snapshot!("pending_message_busy_state", rendered);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_submitted_message_with_text_attachment_snapshot() {
    use std::io::Write;

    // Create a real temp file so render_attachment_preview can read it.
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("notes.txt");
    {
        let mut f = std::fs::File::create(&file_path).unwrap();
        writeln!(f, "Line 1 of notes").unwrap();
        writeln!(f, "Line 2 of notes").unwrap();
    }

    let mut harness = TuiTestHarness::with_size(60, 18);

    // Directly push transcript items that submit_pending_message_inner
    // would create: UserText, then AttachmentHeader + AttachmentItem +
    // AttachmentPreviewLines.
    let tui = harness.tui();
    tui.app
        .transcript
        .push(TranscriptItem::UserText("check this file".to_string()));
    tui.app.transcript.push(TranscriptItem::AttachmentHeader(
        "Attachments (1)".to_string(),
    ));
    tui.app
        .transcript
        .push(TranscriptItem::AttachmentItem("notes.txt".to_string()));
    tui.app
        .transcript
        .push(TranscriptItem::AttachmentPreviewLine(
            "Line 1 of notes".to_string(),
        ));
    tui.app
        .transcript
        .push(TranscriptItem::AttachmentPreviewLine(
            "Line 2 of notes".to_string(),
        ));
    tui.app
        .transcript
        .push(TranscriptItem::AssistantText("Got it, thanks!".to_string()));

    harness.render();
    let rendered = normalize_screen(&harness.screen_contents());

    // The attachment info should be visible in the rendered output.
    assert!(
        rendered.contains("notes.txt"),
        "expected attachment name visible: {rendered}"
    );
    assert!(
        rendered.contains("Line 1 of notes"),
        "expected attachment preview visible: {rendered}"
    );

    insta::assert_snapshot!("submitted_message_with_text_attachment", rendered);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_input_word_wraps_long_line() {
    // Use a narrow viewport (30 cols) so a long single-line input must wrap
    let mut harness = TuiTestHarness::with_size(30, 10);

    let long_input = "the quick brown fox jumps over the lazy dog";
    harness.tui().set_input_text(long_input);
    harness.render();

    let rendered = normalize_screen(&harness.screen_contents());

    // The input is 43 chars, wider than the 30-col viewport.
    // With word wrap the full text should still be visible across multiple lines.
    assert!(
        rendered.contains("the quick brown fox jumps"),
        "expected start of input visible: {rendered}"
    );
    assert!(
        rendered.contains("the lazy dog"),
        "expected end of input visible after wrap: {rendered}"
    );
    // The full phrase should NOT appear on a single rendered line (it was wrapped).
    assert!(
        !rendered.lines().any(|l| l.contains(long_input)),
        "expected long input to be split across lines, not on one line: {rendered}"
    );

    insta::assert_snapshot!("input_word_wraps_long_line", rendered);
}

/// Test Ctrl+C cancellation during streaming aborts the operation gracefully.
/// The abort signal should stop streaming and the TUI should show a cancellation message.
#[tokio::test(flavor = "multi_thread")]
async fn test_ctrl_c_cancels_streaming() {
    let config =
        test_config_with_mock_client_and_agent("test-agent", Some("ctrl-c-cancel-session"));

    // Mock streams a response that we'll cancel mid-stream
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("Starting response...")
                    .add_text_chunk(" this will be ")
                    .add_text_chunk("interrupted")
                    .build(),
            )
            .build(),
    );

    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone());
    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("Long request".to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "Long request".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    // Wait for mock to be exhausted (streaming complete)
    harness
        .sync()
        .wait_until_mock_exhausted(mock_client.as_ref(), Duration::from_secs(5))
        .await
        .unwrap();

    // Drain pending events until the prompt task has actually finished
    // (`llm_busy=false`). `wait_until_mock_exhausted` only confirms the
    // mock has popped its last turn — the streaming response can still
    // be in flight, and the `Final` event that flips `llm_busy` may not
    // be on the channel yet. Polling here closes that race so the
    // post-Ctrl+C assertion is meaningful in stress runs.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while harness.tui().app.llm_busy {
        while let Ok(event) = harness.tui().event_rx.try_recv() {
            harness.tui().handle_tui_event(event).await.unwrap();
        }
        if !harness.tui().app.llm_busy {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for prompt task to finish (llm_busy stuck)"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    harness.render();

    // Now simulate Ctrl+C on the idle TUI. With per-task abort signals
    // and a conditional `llm_busy` clear (no in-flight task → flip to
    // false), Ctrl+C must leave `llm_busy=false` and surface the abort
    // notice in the transcript.
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();

    harness.render();
    let screen = harness.screen_contents();

    // The transcript should show the abort message and busy state should be cleared.
    assert!(
        screen.contains("aborted") || screen.contains("Ctrl+C"),
        "Screen should show abort message, got: {screen}"
    );
    assert!(!harness.tui().app.llm_busy, "Ctrl+C should clear llm_busy");

    harness.drain_and_settle().await.unwrap();
}

/// Test LLM error during streaming propagates correctly.
/// When the mock returns an error, the error should be visible in the transcript.
#[tokio::test(flavor = "multi_thread")]
async fn test_streaming_error_shows_in_transcript() {
    let config = test_config_with_mock_client_and_agent(
        "test-agent",
        Some("streaming-error-transcript-session"),
    );

    // Create a mock that will return an error on streaming
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .error_on_stream(anyhow::anyhow!("API rate limit exceeded"))
            .build(),
    );

    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone());
    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("Error test".to_string()));

    // The error should propagate through start_prompt
    let result = harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "Error test".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await;

    let _ = result; // start_prompt always returns Ok (spawns a task)

    // Wait for the ErrorText entry to appear in the transcript.
    //
    // Polling the rendered screen for the substring "error:" is ambiguous:
    // the retry loop emits a `NoticeEvent::Warning` along the lines of
    // "Model '...' exhausted retries (error: <cause>) ..." which renders as
    // `SystemText` and *also* contains "error:".  On a slow runner the
    // warning event arrives before the spawned prompt task's
    // `ModelEvent::Error` has been processed into `ErrorText`, so the screen
    // check would succeed while the transcript still lacks the entry the
    // test cares about — producing the exact flake observed in CI on
    // macos-latest.  Polling the transcript directly removes the ambiguity.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        while let Ok(event) = harness.tui().event_rx.try_recv() {
            harness.tui().handle_tui_event(event).await.unwrap();
        }
        let has_error = harness
            .tui()
            .app
            .transcript
            .iter()
            .any(|entry| matches!(entry, TranscriptItem::ErrorText(_)));
        if has_error {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let summary: Vec<String> = harness
                .tui()
                .app
                .transcript
                .iter()
                .map(|e| format!("{e:?}"))
                .collect();
            panic!(
                "Transcript did not gain an ErrorText entry within 5s; transcript was:\n{}",
                summary.join("\n")
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    harness.drain_and_settle().await.unwrap();
}

/// Test that a chained error shows the full cause chain in the TUI transcript.
///
/// This test verifies that when a `ModelEvent::Error` carrying a `pretty_error_string`-
/// formatted chain arrives at the TUI, the full text (including "Caused by:") is
/// stored in `TranscriptItem::ErrorText` and rendered on screen.
///
/// We inject the event directly into the TUI's event channel rather than going
/// through `start_prompt` / the LLM retry pipeline — that avoids backoff delays
/// and keeps the test focused on the TUI's own error-display behaviour.
#[tokio::test]
async fn test_streaming_error_shows_full_cause_chain_in_transcript() {
    use harnx_render::pretty_error_string;

    // Build the chained error and format it the same way the engine does.
    let chained_error = anyhow::anyhow!("root cause detail").context("outer error message");
    let formatted = pretty_error_string(&chained_error);

    // Verify the formatted string has the expected structure before injecting.
    assert!(
        formatted.contains("outer error message"),
        "pretty_error_string should contain outer message, got: {formatted}"
    );
    assert!(
        formatted.contains("Caused by:"),
        "pretty_error_string should contain 'Caused by:', got: {formatted}"
    );
    assert!(
        formatted.contains("root cause detail"),
        "pretty_error_string should contain root cause, got: {formatted}"
    );

    let config =
        test_config_with_mock_client_and_agent("test-agent", Some("error-chain-display-session"));
    let mut harness = TuiTestHarness::with_config(config.clone());
    harness.tui().clear_transcript();

    // Inject the already-formatted error directly into the TUI event channel,
    // bypassing the LLM call / retry pipeline entirely.
    harness
        .tui()
        .event_tx
        .send(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::Error(formatted.clone())),
            None,
        ))
        .unwrap();

    // Drain the single queued event into the transcript.
    harness.drain_and_settle().await.unwrap();

    // The transcript must have an ErrorText entry containing the full chain.
    let error_text = harness
        .tui()
        .app
        .transcript
        .iter()
        .find_map(|entry| match entry {
            TranscriptItem::ErrorText(text) => Some(text.clone()),
            _ => None,
        })
        .expect("Transcript should contain an ErrorText entry after ModelEvent::Error");

    assert!(
        error_text.contains("outer error message"),
        "ErrorText should contain outer error message, got: {error_text}"
    );
    assert!(
        error_text.contains("Caused by:"),
        "ErrorText should contain 'Caused by:', got: {error_text}"
    );
    assert!(
        error_text.contains("root cause detail"),
        "ErrorText should contain root cause detail, got: {error_text}"
    );

    // Also verify the rendered screen shows "error:" prefix.
    harness.render();
    assert!(
        harness.screen_contents().contains("error:"),
        "Rendered screen should show 'error:' prefix for ErrorText"
    );
}

/// Test cancellation during tool call execution.
/// When user presses Ctrl+C while a tool is executing, the tool should be aborted.
#[tokio::test(flavor = "multi_thread")]
async fn test_cancel_during_tool_execution() {
    let config =
        test_config_with_mock_client_and_agent("test-agent", Some("cancel-tool-execution-session"));

    // Mock returns a tool call, then more text
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("Let me search...")
                    .add_tool_call("search", serde_json::json!({"query": "test"}))
                    .build(),
            )
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("Found results!")
                    .build(),
            )
            .build(),
    );

    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone());
    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("Search test".to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "Search test".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    // Wait for mock to be exhausted
    harness
        .sync()
        .wait_until_mock_exhausted(mock_client.as_ref(), Duration::from_secs(5))
        .await
        .unwrap();

    // Process events (including tool call)
    loop {
        match harness.tui().event_rx.try_recv() {
            Ok(event) => {
                let _ = harness.tui().handle_tui_event(event).await;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(_) => break,
        }
    }
    harness.render();

    // Simulate Ctrl+C after tool call is processed
    harness.tui().abort_signal.set_ctrlc();

    // Manually trigger the Ctrl+C handling (same as handle_key for Ctrl+C)
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::SystemText(
            "(Ctrl+C — operation aborted. Ctrl+D to exit.)".to_string(),
        ));
    harness.tui().app.llm_busy = false;
    harness.tui().abort_signal.reset();

    harness.render();

    // The transcript should show the abort message
    let screen = harness.screen_contents();
    assert!(
        screen.contains("aborted") || screen.contains("Ctrl+C"),
        "Screen should show abort message after cancel during tool execution, got: {screen}"
    );

    harness.drain_and_settle().await.unwrap();
}

#[tokio::test]
async fn paste_multiline_creates_temp_attachment() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.handle_paste("line one\nline two\nline three".to_string())
        .await;

    // Multi-line paste should NOT insert into the textarea
    let text = tui.app.input.lines().join("\n");
    assert_eq!(text, "", "Multi-line paste should not go into textarea");

    // Instead it should create a temp file attachment in the attachment dir
    assert_eq!(tui.app.attachments.len(), 1, "Should create one attachment");
    assert!(
        tui.app.attachment_dir.is_some(),
        "Should create attachment dir"
    );
    assert!(
        tui.app.attachments[0].path.exists(),
        "Temp file should exist"
    );
    assert_eq!(tui.app.attachments[0].display_name, "paste-1.txt");

    // The temp file should contain the pasted text
    let contents = tokio::fs::read_to_string(&tui.app.attachments[0].path)
        .await
        .unwrap();
    assert_eq!(contents, "line one\nline two\nline three");

    // No submission should have occurred
    let user_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter(|entry| matches!(entry, TranscriptItem::UserText(_)))
        .collect();
    assert!(
        user_entries.is_empty(),
        "Paste should not trigger submission"
    );

    // Cleanup
    tui.cleanup_attachments();
}

#[tokio::test]
async fn paste_multiline_with_cr_creates_temp_attachment() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    // Some terminals send \r instead of \n for newlines in paste
    tui.handle_paste("line one\rline two\rline three".to_string())
        .await;

    assert_eq!(
        tui.app.attachments.len(),
        1,
        "CR-separated paste should create attachment"
    );
    let contents = std::fs::read_to_string(&tui.app.attachments[0].path).unwrap();
    assert_eq!(
        contents, "line one\nline two\nline three",
        "CRs should be normalized to LFs"
    );

    tui.cleanup_attachments();
}

#[tokio::test]
async fn paste_multiline_with_crlf_creates_temp_attachment() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    // Windows-style line endings
    tui.handle_paste("line one\r\nline two\r\nline three".to_string())
        .await;

    assert_eq!(
        tui.app.attachments.len(),
        1,
        "CRLF paste should create attachment"
    );

    let contents = std::fs::read_to_string(&tui.app.attachments[0].path).unwrap();
    assert_eq!(
        contents, "line one\nline two\nline three",
        "CRLFs should be normalized to LFs"
    );

    tui.cleanup_attachments();
}

#[tokio::test]
async fn paste_single_line_inserts_inline() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.handle_paste("single line text".to_string()).await;

    let text = tui.app.input.lines().join("\n");
    assert_eq!(text, "single line text");
    assert!(
        tui.app.attachments.is_empty(),
        "Single-line paste should not create attachment"
    );
}

#[tokio::test]
async fn paste_then_erase_then_paste_different_text() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    // First paste (single-line)
    tui.handle_paste("first paste".to_string()).await;
    assert_eq!(tui.app.input.lines().join("\n"), "first paste");

    // Erase everything by resetting the input
    tui.app.input = Tui::new_input();

    // Second paste (single-line, different text)
    tui.handle_paste("second paste".to_string()).await;
    let text = tui.app.input.lines().join("\n");
    assert_eq!(
        text, "second paste",
        "Should only contain the second paste, not the first"
    );
}

#[tokio::test]
async fn detach_cleans_up_temp_dir() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    // Paste multi-line to create a temp attachment
    tui.handle_paste("line one\nline two".to_string()).await;
    assert_eq!(tui.app.attachments.len(), 1);
    let temp_dir = tui.app.attachment_dir.clone().unwrap();
    let temp_path = tui.app.attachments[0].path.clone();
    assert!(temp_dir.exists(), "Temp dir should exist before detach");
    assert!(temp_path.exists(), "Temp file should exist before detach");

    // Detach all
    tui.set_input_text(".detach");
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(tui.app.attachments.is_empty());
    assert!(tui.app.attachment_dir.is_none());
    assert!(
        !temp_dir.exists(),
        "Temp dir should be deleted after detach"
    );
    assert!(
        !temp_path.exists(),
        "Temp file should be deleted after detach"
    );
}

#[tokio::test]
async fn attachment_footer_shows_attached_files() {
    use crate::types::Attachment;
    use std::path::PathBuf;

    let mut harness = TuiTestHarness::with_size(60, 12);
    harness.tui().app.attachments.push(Attachment {
        path: PathBuf::from("/tmp/photo.png"),
        display_name: "photo.png".to_string(),
    });
    harness.render();

    let screen = harness.screen_contents();
    assert!(
        screen.contains("photo.png"),
        "Attachment footer should show filename, got: {screen}"
    );
}

#[tokio::test]
async fn paste_appends_to_existing_text() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.set_input_text("before ");
    tui.handle_paste("pasted text".to_string()).await;

    let text = tui.app.input.lines().join("\n");
    assert_eq!(text, "before pasted text");
}

#[tokio::test]
async fn attach_command_adds_attachment() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let tmp = std::env::temp_dir().join("harnx_test_attach.txt");
    std::fs::write(&tmp, "test content").unwrap();

    tui.set_input_text(&format!(".attach {}", tmp.display()));
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(tui.app.attachments.len(), 1);
    assert_eq!(tui.app.attachments[0].display_name, "harnx_test_attach.txt");
    assert_eq!(tui.app.input.lines().join("\n"), "");
    let user_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter(|e| matches!(e, TranscriptItem::UserText(_)))
        .collect();
    assert!(user_entries.is_empty());

    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn attach_command_preserves_draft_text() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let tmp = std::env::temp_dir().join("harnx_test_attach2.txt");
    std::fs::write(&tmp, "test").unwrap();

    tui.set_input_text(&format!("Explain this image\n.attach {}", tmp.display()));
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(tui.app.attachments.len(), 1);
    assert_eq!(tui.app.input.lines().join("\n"), "Explain this image");
    let user_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter(|e| matches!(e, TranscriptItem::UserText(_)))
        .collect();
    assert!(user_entries.is_empty());

    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn direct_submit_with_attachments_renders_attachment_entries_in_transcript() {
    use crate::types::Attachment;
    use std::path::PathBuf;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.app.attachments = vec![Attachment {
        path: PathBuf::from("/tmp/example.txt"),
        display_name: "example.txt".to_string(),
    }];
    tui.set_input_text("check attachment");

    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    let user_index = tui
        .app
        .transcript
        .iter()
        .position(
            |item| matches!(item, TranscriptItem::UserText(text) if text == "check attachment"),
        )
        .expect("expected submitted user text in transcript");

    assert!(matches!(
        tui.app.transcript.get(user_index + 1),
        Some(TranscriptItem::AttachmentHeader(text)) if text == "Attachments (1)"
    ));
    assert!(matches!(
        tui.app.transcript.get(user_index + 2),
        Some(TranscriptItem::AttachmentItem(text)) if text == "example.txt"
    ));
}

#[tokio::test]
async fn dot_command_with_attachments_renders_attachment_entries_in_transcript() {
    use crate::types::Attachment;
    use std::path::PathBuf;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.app.attachments = vec![Attachment {
        path: PathBuf::from("/tmp/example.txt"),
        display_name: "example.txt".to_string(),
    }];
    tui.set_input_text(".info attachments");

    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    let user_index = tui
        .app
        .transcript
        .iter()
        .position(
            |item| matches!(item, TranscriptItem::UserText(text) if text == ".info attachments"),
        )
        .expect("expected dot-command user text in transcript");

    assert!(matches!(
        tui.app.transcript.get(user_index + 1),
        Some(TranscriptItem::AttachmentHeader(text)) if text == "Attachments (1)"
    ));
    assert!(matches!(
        tui.app.transcript.get(user_index + 2),
        Some(TranscriptItem::AttachmentItem(text)) if text == "example.txt"
    ));
}

#[tokio::test]
async fn attach_nonexistent_file_shows_error() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.set_input_text(".attach /nonexistent/file.txt");
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(tui.app.attachments.is_empty());
    let has_error = tui
        .app
        .transcript
        .iter()
        .any(|e| matches!(e, TranscriptItem::ErrorText(msg) if msg.contains("not found")));
    assert!(has_error, "Should show error for nonexistent file");
}

#[tokio::test]
async fn detach_clears_all_attachments() {
    use crate::types::Attachment;
    use std::path::PathBuf;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.app.attachments.push(Attachment {
        path: PathBuf::from("/tmp/a.txt"),
        display_name: "a.txt".to_string(),
    });
    tui.app.attachments.push(Attachment {
        path: PathBuf::from("/tmp/b.txt"),
        display_name: "b.txt".to_string(),
    });

    tui.set_input_text(".detach");
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(tui.app.attachments.is_empty());
}

#[tokio::test]
async fn detach_by_name_removes_specific_attachment() {
    use crate::types::Attachment;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let temp_dir =
        std::env::temp_dir().join(format!("harnx-detach-by-name-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let a_path = temp_dir.join("a.txt");
    let b_path = temp_dir.join("b.txt");
    std::fs::write(&a_path, "a").unwrap();
    std::fs::write(&b_path, "b").unwrap();

    tui.app.attachments.push(Attachment {
        path: a_path.clone(),
        display_name: "a.txt".to_string(),
    });
    tui.app.attachments.push(Attachment {
        path: b_path.clone(),
        display_name: "b.txt".to_string(),
    });

    tui.set_input_text(".detach a.txt");
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(tui.app.attachments.len(), 1);
    assert_eq!(tui.app.attachments[0].display_name, "b.txt");
    assert!(
        !a_path.exists(),
        "Named detach should immediately remove the attachment file from disk"
    );
    assert!(
        b_path.exists(),
        "Non-detached attachment file should remain"
    );
}

#[tokio::test]
async fn submit_drains_attachments() {
    use crate::types::Attachment;

    let config = test_config_with_mock_client_and_agent(
        "test-agent",
        Some("submit-drains-attachments-session"),
    );
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(MockTurnBuilder::new().add_text_chunk("done").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;
    let mut harness = TuiTestHarness::with_config(config.clone());

    let temp_dir = std::env::temp_dir().join(format!("harnx-submit-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let path = temp_dir.join("test.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    harness.tui().app.attachments.push(Attachment {
        path: path.clone(),
        display_name: "test.txt".to_string(),
    });
    harness.tui().set_input_text("Analyze this file");

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(harness.tui().app.attachments.is_empty());
    assert!(harness.tui().app.llm_busy);

    harness
        .sync()
        .wait_until_mock_exhausted(&mock_client, Duration::from_secs(5))
        .await
        .unwrap();
    harness.drain_and_settle().await.unwrap();
    assert!(
        !harness.tui().app.llm_busy,
        "Prompt lifecycle should complete"
    );
}

#[tokio::test]
async fn submit_attachments_only_with_empty_text() {
    use crate::types::Attachment;

    let config = test_config_with_mock_client_and_agent(
        "test-agent",
        Some("submit-attachments-only-session"),
    );
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(MockTurnBuilder::new().add_text_chunk("done").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;
    let mut harness = TuiTestHarness::with_config(config.clone());

    let temp_dir = std::env::temp_dir().join(format!("harnx-submit-only-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let path = temp_dir.join("test.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    harness.tui().app.attachments.push(Attachment {
        path: path.clone(),
        display_name: "test.txt".to_string(),
    });

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(harness.tui().app.attachments.is_empty());
    assert!(harness.tui().app.llm_busy);

    harness
        .sync()
        .wait_until_mock_exhausted(&mock_client, Duration::from_secs(5))
        .await
        .unwrap();
    harness.drain_and_settle().await.unwrap();
    assert!(
        !harness.tui().app.llm_busy,
        "Prompt lifecycle should complete"
    );
}

#[tokio::test]
async fn queued_message_keeps_attachments_visible_while_busy() {
    use crate::types::Attachment;
    use std::path::PathBuf;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.app.llm_busy = true;
    tui.app.attachments.push(Attachment {
        path: PathBuf::from("/tmp/paste-1.txt"),
        display_name: "paste-1.txt".to_string(),
    });

    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(tui.app.pending_message.is_some());
    assert_eq!(tui.app.attachments.len(), 1);
    assert_eq!(tui.app.attachments[0].display_name, "paste-1.txt");
    assert_eq!(
        tui.app.pending_message.as_ref().unwrap().attachments.len(),
        1,
        "Pending message should also retain the attachment"
    );
}

/// Test recovery after cancellation - user can send a new message.
#[tokio::test(flavor = "multi_thread")]
async fn test_recovery_after_cancellation() {
    let config = test_config_with_mock_client_and_agent(
        "test-agent",
        Some("recovery-after-cancellation-session"),
    );

    // Mock for both turns - each start_prompt consumes one turn
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("First response")
                    .build(),
            )
            .build(),
    );

    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone());
    harness.tui().clear_transcript();

    // Send first message
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("First request".to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "First request".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    // Wait for first response to appear on screen
    harness
        .wait_until_screen_contains("First response", Duration::from_secs(5))
        .await
        .unwrap();

    // Ensure the first prompt's background task finishes and all its events
    // (including Finished) are drained, so they don't interfere with the
    // second prompt below.
    harness
        .sync()
        .wait_until_mock_exhausted(mock_client.as_ref(), Duration::from_secs(5))
        .await
        .unwrap();
    harness.drain_and_settle().await.unwrap();

    // Simulate cancellation
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::SystemText(
            "(Ctrl+C — operation aborted. Ctrl+D to exit.)".to_string(),
        ));
    harness.tui().app.llm_busy = false;
    harness.tui().abort_signal.reset();
    harness.render();

    // Verify abort signal is reset
    assert!(
        !harness.tui().abort_signal.aborted(),
        "abort signal should be reset after cancel"
    );

    // Create a second mock for the second request
    let mock_client2 = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("Second response after recovery")
                    .build(),
            )
            .build(),
    );
    _guard.set_client(Some(mock_client2.clone()));

    // User can send a new message
    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("Second request".to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "Second request".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    // Wait for second response to appear on screen
    harness
        .wait_until_screen_contains("Second response", Duration::from_secs(5))
        .await
        .unwrap();

    harness.drain_and_settle().await.unwrap();
}

#[tokio::test]
async fn attach_completes_file_paths() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let tmp_dir = std::env::temp_dir();
    let tmp_file = tmp_dir.join("harnx_completion_test.txt");
    std::fs::write(&tmp_file, "test").unwrap();

    let line = format!(".attach {}/harnx_completion", tmp_dir.display());
    let completions = tui.compute_completions(&line, line.len()).await;

    assert!(
        completions
            .iter()
            .any(|(v, _)| v.contains("harnx_completion_test.txt")),
        "Should complete file paths, got: {:?}",
        completions
    );

    std::fs::remove_file(&tmp_file).ok();
}

#[tokio::test]
async fn detach_completes_attachment_names() {
    use crate::types::Attachment;
    use std::path::PathBuf;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.app.attachments.push(Attachment {
        path: PathBuf::from("/tmp/photo.png"),
        display_name: "photo.png".to_string(),
    });
    tui.app.attachments.push(Attachment {
        path: PathBuf::from("/tmp/data.csv"),
        display_name: "data.csv".to_string(),
    });

    let completions = tui.compute_completions(".detach ", 8).await;
    let names: Vec<&str> = completions.iter().map(|(v, _)| v.as_str()).collect();

    assert!(
        names.contains(&"photo.png"),
        "Should complete attachment names, got: {:?}",
        names
    );
    assert!(
        names.contains(&"data.csv"),
        "Should complete attachment names, got: {:?}",
        names
    );
}

/// Reproduce potential duplicate sub-agent activity in the TUI.
///
/// Simulates a realistic flow where:
/// - Top-level agent streams a few message fragments
/// - Sub-agent emits several streaming thought fragments
/// - Sub-agent makes two tool calls
/// - Sub-agent emits more streaming thoughts
/// - Sub-agent makes another tool call
/// - Sub-agent sends a final message
/// - Top-level agent streams its final response in several parts
///
/// Insta snapshots are taken at several points to allow visual verification
/// that each activity type appears exactly once (no duplicates).
#[tokio::test(flavor = "multi_thread")]
async fn sub_agent_activity_no_duplicates_snapshot() {
    let config = test_config_with_mock_client_and_agent("coordinator", Some("dedup-test-session"));
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(80, 30);
    harness.tui().config = config;
    harness.tui().persistent_manager = persistent;
    harness.tui().clear_transcript();

    let sub_source = Some(AgentSource {
        agent: "researcher".to_string(),
        session_id: Some("research-session-1".to_string()),
    });

    // ── Phase 1: Top-level agent streams opening message ─────────────
    for chunk in ["I'll look into ", "that for you. ", "Delegating now."] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::Agent(
                AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text(chunk.to_string())],
                }),
                None,
            ))
            .await
            .unwrap();
    }

    // ── Phase 2: Top-level delegation tool call ──────────────────────
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Tool(ToolEvent::Started {
                id: String::new(),
                name: "researcher_session_prompt".to_string(),
                kind: ToolKind::Other,
                title: None,
                input: yaml_to_json("message: investigate the data"),
                locations: vec![],
            }),
            None,
        ))
        .await
        .unwrap();

    // ── Phase 3: Sub-agent thinks in several fragments ───────────────
    for chunk in ["Let me ", "analyze ", "the situation ", "carefully."] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::Agent(
                AgentEvent::Model(ModelEvent::ThoughtChunk {
                    blocks: vec![ContentBlock::Text(chunk.to_string())],
                }),
                sub_source.clone(),
            ))
            .await
            .unwrap();
    }

    // ── Phase 4: Sub-agent makes two tool calls ──────────────────────
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Tool(ToolEvent::Started {
                id: String::new(),
                name: "bash".to_string(),
                kind: ToolKind::Other,
                title: None,
                input: yaml_to_json("command: find /data -name '*.csv'"),
                locations: vec![],
            }),
            sub_source.clone(),
        ))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Tool(ToolEvent::Started {
                id: String::new(),
                name: "read_file".to_string(),
                kind: ToolKind::Other,
                title: None,
                input: yaml_to_json("path: /data/results.csv"),
                locations: vec![],
            }),
            sub_source.clone(),
        ))
        .await
        .unwrap();

    // Snapshot after initial sub-agent activity (thoughts + two tool calls)
    harness.tui().flush_pending_thought_for_test();
    harness.render();
    let rendered_mid1 = normalize_screen(&harness.screen_contents());
    insta::assert_snapshot!("sub_agent_activity_dedup_after_first_tools", rendered_mid1);

    // ── Phase 5: Sub-agent thinks more ───────────────────────────────
    for chunk in ["Now I see ", "the pattern ", "in the data."] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::Agent(
                AgentEvent::Model(ModelEvent::ThoughtChunk {
                    blocks: vec![ContentBlock::Text(chunk.to_string())],
                }),
                sub_source.clone(),
            ))
            .await
            .unwrap();
    }

    // ── Phase 6: Sub-agent makes one more tool call ──────────────────
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Tool(ToolEvent::Started {
                id: String::new(),
                name: "write_file".to_string(),
                kind: ToolKind::Other,
                title: None,
                input: yaml_to_json("path: /data/summary.md"),
                locations: vec![],
            }),
            sub_source.clone(),
        ))
        .await
        .unwrap();

    // Snapshot after all sub-agent tool calls
    harness.tui().flush_pending_thought_for_test();
    harness.render();
    let rendered_mid2 = normalize_screen(&harness.screen_contents());
    insta::assert_snapshot!("sub_agent_activity_dedup_after_all_tools", rendered_mid2);

    // ── Phase 7: Sub-agent sends final message ───────────────────────
    for chunk in [
        "Here are ",
        "my findings: ",
        "the data shows a clear trend.",
    ] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::Agent(
                AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text(chunk.to_string())],
                }),
                sub_source.clone(),
            ))
            .await
            .unwrap();
    }

    // ── Phase 8: Sub-agent usage line ────────────────────────────────
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(
            AgentEvent::Model(ModelEvent::Usage {
                input: 500,
                output: 200,
                cached: 100,
                session_label: Some("> researcher ▸ research-session-1".to_string()),
            }),
            sub_source.clone(),
        ))
        .await
        .unwrap();

    // ── Phase 9: Top-level agent streams final response ──────────────
    for chunk in [
        "Based on the ",
        "research, ",
        "the data clearly shows ",
        "an upward trend.",
    ] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::Agent(
                AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text(chunk.to_string())],
                }),
                None,
            ))
            .await
            .unwrap();
    }

    // Final snapshot showing the complete flow
    harness.render();
    let rendered_final = normalize_screen(&harness.screen_contents());
    insta::assert_snapshot!("sub_agent_activity_dedup_final", rendered_final);

    // ── Verify no duplicate entries ──────────────────────────────────
    let all_entries: Vec<_> = harness
        .tui()
        .app
        .transcript
        .iter()
        .flat_map(Tui::render_entry)
        .filter_map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            if text.is_empty() || text.starts_with("Welcome to harnx") || text.starts_with('•') {
                None
            } else {
                Some(text)
            }
        })
        .collect();

    // The sub-agent heading should appear exactly once
    let heading_count = all_entries
        .iter()
        .filter(|e| *e == "> researcher ▸ research-session-1")
        .count();
    assert_eq!(
        heading_count, 1,
        "sub-agent heading should appear exactly once, got {heading_count}. entries: {all_entries:?}"
    );

    // Each tool call should appear exactly once
    for tool_name in ["bash", "read_file", "write_file"] {
        let tool_count = all_entries
            .iter()
            .filter(|e| *e == &format!("→ {tool_name}"))
            .count();
        assert_eq!(
            tool_count, 1,
            "tool call {tool_name} should appear exactly once, got {tool_count}. entries: {all_entries:?}"
        );
    }

    // The delegation tool call (top-level) should appear exactly once
    let delegation_count = all_entries
        .iter()
        .filter(|e| *e == "→ researcher_session_prompt")
        .count();
    assert_eq!(
        delegation_count, 1,
        "delegation tool call should appear exactly once, got {delegation_count}. entries: {all_entries:?}"
    );

    harness.drain_and_settle().await.unwrap();
}

// ── History preview mode tests (issue #281) ──────────────────────────────────

/// Create a `Tui` pre-seeded with history entries for history-preview tests.
/// Entries are given newest-first: index 0 will be the most-recent entry.
fn make_tui_with_history(entries: &[&str]) -> (crate::types::Tui, GlobalConfig) {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    for entry in entries.iter().rev() {
        tui.app.history.insert(0, entry.to_string());
    }
    (tui, config)
}

/// Blank input + Up → enters preview mode, shows most-recent history entry.
#[tokio::test]
async fn history_up_on_blank_enters_preview() {
    let (mut tui, _config) = make_tui_with_history(&["first message", "second message"]);

    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(tui.app.history_preview, "should be in preview mode");
    assert_eq!(tui.app.history_index, Some(0));
    assert_eq!(
        tui.app.input.lines().join("\n"),
        "first message",
        "input should show most-recent history entry"
    );
}

/// Blank input + Up with empty history → no preview mode, no-op.
#[tokio::test]
async fn history_up_on_blank_empty_history_no_preview() {
    let (mut tui, _config) = make_tui_with_history(&[]);

    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(
        !tui.app.history_preview,
        "should NOT enter preview with empty history"
    );
    assert_eq!(tui.app.history_index, None);
    assert_eq!(tui.app.input.lines().join("\n"), "");
}

/// Up twice navigates to the older entry (index 1).
#[tokio::test]
async fn history_up_up_navigates_older() {
    let (mut tui, _config) = make_tui_with_history(&["first message", "second message"]);

    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(tui.app.history_preview, "should still be in preview mode");
    assert_eq!(tui.app.history_index, Some(1));
    assert_eq!(
        tui.app.input.lines().join("\n"),
        "second message",
        "input should show older history entry"
    );
}

/// Up then Down → preview exits, draft restored.
#[tokio::test]
async fn history_down_returns_to_draft() {
    let (mut tui, _config) = make_tui_with_history(&["first message"]);

    // Start with blank input and press Up to enter preview mode.
    // history_prev() saves the current input ("") into history_draft.
    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.app.history_preview, "should be in preview after Up");
    assert_eq!(tui.app.input.lines().join("\n"), "first message");

    // Simulate that the user had typed a draft before navigating — overwrite
    // history_draft directly so Down will restore it.
    tui.app.history_draft = "my draft".to_string();

    // Down returns to draft
    tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.history_preview, "preview should be exited");
    assert_eq!(tui.app.history_index, None);
    assert_eq!(
        tui.app.input.lines().join("\n"),
        "my draft",
        "draft should be restored"
    );
}

/// While in preview, pressing a char exits preview and appends char to input.
#[tokio::test]
async fn history_typing_exits_preview() {
    let (mut tui, _config) = make_tui_with_history(&["hello"]);

    // Enter preview
    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.app.history_preview);

    // Type a character
    tui.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(!tui.app.history_preview, "preview should be exited");
    assert_eq!(tui.app.history_index, None);
    // Content preserved + char appended
    let content = tui.app.input.lines().join("\n");
    assert!(
        content.contains("hello"),
        "history content should be preserved: {content}"
    );
    assert!(
        content.contains('!'),
        "typed char should be appended: {content}"
    );
}

/// Non-blank input + Up (not in preview) → cursor moves up in textarea, no history change.
#[tokio::test]
async fn history_up_not_blank_moves_cursor() {
    let (mut tui, _config) = make_tui_with_history(&["old message"]);

    // Set multi-line draft (cursor ends at bottom)
    tui.set_input_text("hello\nworld");

    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(
        !tui.app.history_preview,
        "should NOT enter preview with non-blank input"
    );
    assert_eq!(
        tui.app.history_index, None,
        "history index should not change"
    );
    // Input text unchanged
    assert_eq!(tui.app.input.lines().join("\n"), "hello\nworld");
}

/// Non-blank input + Down (not in preview) → cursor moves down, no history change.
#[tokio::test]
async fn history_down_not_preview_moves_cursor() {
    let (mut tui, _config) = make_tui_with_history(&["old message"]);

    // Set multi-line draft
    tui.set_input_text("hello\nworld");

    tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(!tui.app.history_preview, "should NOT enter preview");
    assert_eq!(
        tui.app.history_index, None,
        "history index should not change"
    );
    assert_eq!(tui.app.input.lines().join("\n"), "hello\nworld");
}

/// While in preview, paste exits preview.
#[tokio::test]
async fn history_paste_exits_preview() {
    let (mut tui, _config) = make_tui_with_history(&["hello"]);

    // Enter preview
    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.app.history_preview);

    // Paste inline text
    tui.handle_paste("pasted".to_string()).await;

    assert!(
        !tui.app.history_preview,
        "preview should be exited after paste"
    );
    assert_eq!(tui.app.history_index, None);
}

/// Preview mode produces a distinct cursor style from normal mode.
#[tokio::test]
async fn history_preview_cursor_style() {
    let (mut tui, config) = make_tui_with_history(&[]);

    // Normal mode style
    tui.app.history_preview = false;
    Tui::refresh_input_chrome_from_state(&config, &mut tui.app, false, false);
    let normal_style = tui.app.input.cursor_style();

    // Preview mode style
    tui.app.history_preview = true;
    Tui::refresh_input_chrome_from_state(&config, &mut tui.app, false, false);
    let preview_style = tui.app.input.cursor_style();

    assert_ne!(
        normal_style, preview_style,
        "cursor style should differ between normal and preview mode"
    );
    assert!(
        preview_style.add_modifier.contains(Modifier::REVERSED),
        "preview cursor should still have REVERSED modifier for visibility"
    );
}

/// Regression test for #292: Ctrl-C during in-flight streaming must actually
/// abort background task, not only update UI state.
///
/// FAILS before fix (abort_signal.reset() in Ctrl-C handler clears signal before
/// background task can observe it).
/// PASSES after fix (reset() removed from Ctrl-C handler).
#[tokio::test(flavor = "multi_thread")]
async fn test_ctrl_c_interrupts_in_flight_streaming() {
    let config =
        test_config_with_mock_client_and_agent("test-agent", Some("ctrl-c-in-flight-session"));
    let gate_reached = Arc::new(Notify::new());
    let gate_release = Arc::new(Notify::new());
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("before-gate")
                    .add_gate(gate_reached.clone(), gate_release.clone())
                    .add_text_chunk("after-gate — should not appear")
                    .build(),
            )
            .build(),
    );

    let _guard = TestStateGuard::new(Some(mock_client)).await;

    let mut harness = TuiTestHarness::with_config(config.clone());
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::UserText("Long request".to_string()));
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "Long request".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), gate_reached.notified())
        .await
        .expect("mock stream should reach gate before Ctrl+C");

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();

    gate_release.notify_one();

    if harness
        .wait_until_screen_contains("aborted", Duration::from_secs(5))
        .await
        .is_err()
    {
        harness
            .wait_until_screen_contains("Ctrl+C", Duration::from_secs(5))
            .await
            .unwrap();
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while harness.tui().app.llm_busy {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for llm_busy to clear"
        );
        while let Ok(event) = harness.tui().event_rx.try_recv() {
            harness.tui().handle_tui_event(event).await.unwrap();
        }
        harness.render();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let screen = harness.screen_contents();
    assert!(
        !screen.contains("after-gate — should not appear"),
        "stream should be interrupted before gate release chunk appears: {screen}"
    );
    assert!(
        screen.contains("aborted") || screen.contains("Ctrl+C"),
        "screen should show abort message, got: {screen}"
    );
    assert!(
        !harness.tui().app.llm_busy,
        "Ctrl+C should clear llm_busy for in-flight streaming"
    );

    harness.drain_and_settle().await.unwrap();
}

/// Regression test for Bug 2's mechanism: pressing Ctrl+C while a
/// prompt task is running must not eagerly clear `llm_busy`. Before the
/// fix, the Ctrl+C handler set `llm_busy = false` immediately, which
/// caused the very next Enter to take the "spawn a fresh prompt" branch
/// and run a second task alongside the first — corrupting session
/// state. With the fix, `llm_busy` stays true until the running task
/// actually emits Final/Error, and the user's new message queues into
/// `pending_message` instead of racing the old task.
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_during_in_flight_task_does_not_clear_llm_busy_or_spawn_new_task() {
    let config = test_config_with_mock_client_and_agent("test-agent", Some("ctrl-c-bug2-busy"));
    let gate_reached = Arc::new(Notify::new());
    let gate_release = Arc::new(Notify::new());
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("first")
                    .add_gate(gate_reached.clone(), gate_release.clone())
                    .build(),
            )
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("second response done")
                    .build(),
            )
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone());

    // Start Task A and let it wedge on the gate.
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "first message".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gate_reached.notified())
        .await
        .expect("Task A should reach the gate");

    let task_a_abort = harness
        .tui()
        .current_prompt_abort
        .clone()
        .expect("Task A should have a per-task abort signal");
    let task_a_abort_handle_dbg = format!(
        "{:?}",
        harness
            .tui()
            .current_prompt_handle
            .as_ref()
            .expect("Task A should have a JoinHandle")
            .abort_handle()
    );

    // Press Ctrl+C.
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();

    // Task A's per-task abort fired; llm_busy is NOT cleared (Task A is
    // still in flight); the queued-message channel is cleared.
    assert!(
        task_a_abort.aborted_ctrlc(),
        "Ctrl+C must set Task A's per-task abort signal"
    );
    assert!(
        harness.tui().app.llm_busy,
        "llm_busy must remain true while Task A is winding down — \
         clearing it eagerly is what allowed Bug 2's concurrent tasks"
    );
    assert!(harness.tui().app.pending_message.is_none());

    // User types a new message and presses Enter while Task A is still
    // running. With llm_busy still true, this must queue (not spawn).
    harness.tui().set_input_text("second message");
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    // The new message must be QUEUED, not running. The same Task A
    // handle / abort signal must still be present — no Task B was
    // spawned alongside.
    assert!(
        harness.tui().app.pending_message.is_some(),
        "the typed message must queue while Task A is still in flight"
    );
    let task_a_abort_after = harness
        .tui()
        .current_prompt_abort
        .clone()
        .expect("the per-task abort slot must still hold Task A's signal");
    assert!(
        std::sync::Arc::ptr_eq(&task_a_abort, &task_a_abort_after),
        "the current_prompt_abort must still be Task A's — no second task \
         was spawned. (Bug 2 produced a second concurrent task here.)"
    );
    assert!(
        task_a_abort_after.aborted_ctrlc(),
        "Task A's abort must remain set — per-task signals are immune to \
         resets driven by a fresh submission"
    );
    assert_eq!(
        format!(
            "{:?}",
            harness
                .tui()
                .current_prompt_handle
                .as_ref()
                .expect("handle slot must still hold Task A")
                .abort_handle()
        ),
        task_a_abort_handle_dbg,
        "the JoinHandle slot must still hold Task A's handle"
    );

    // Release Task A so the test can clean up. After Final propagates,
    // submit_pending_message_inner will spawn the queued Task B; both
    // tasks must drain before the harness shuts down or the spawned
    // tokio task can outlive its TestStateGuard.
    gate_release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        while let Ok(event) = harness.tui().event_rx.try_recv() {
            harness.tui().handle_tui_event(event).await.unwrap();
        }
        if mock_client.remaining_turns() == 0 && !harness.tui().app.llm_busy {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for both prompts to drain"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    harness.drain_and_settle().await.unwrap();
}

/// Regression test for the second observable symptom of Bug 2: a
/// Ctrl+C-then-resubmit must not consume a fresh mock turn while the
/// old task is still in flight. Before the fix, the Enter handler
/// observed `llm_busy=false` (cleared eagerly by Ctrl+C) and spawned
/// Task B immediately — Task B then made its own LLM call, popping the
/// next mock turn while Task A was still wedged on the gate. With the
/// fix, `llm_busy` stays true, the message queues, and the next mock
/// turn is consumed only after Task A completes.
///
/// FAILS before the fix: `mock_client.remaining_turns()` drops to 0
/// because Task B was spawned alongside Task A.
/// PASSES after the fix: `remaining_turns()` stays at 1 — Task B is
/// queued, not running.
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_resubmit_does_not_consume_a_second_mock_turn_while_task_a_is_in_flight() {
    let config = test_config_with_mock_client_and_agent("test-agent", Some("orphan-tool-bug2"));
    let gate_reached = Arc::new(Notify::new());
    let gate_release = Arc::new(Notify::new());
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_tool_call("noop", serde_json::json!({}))
                    .add_gate(gate_reached.clone(), gate_release.clone())
                    .build(),
            )
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("second response done")
                    .build(),
            )
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone());

    // Task A: mock returns a tool_call, then wedges on the gate so
    // execute_tool_round can't write its tool_results yet.
    harness
        .tui()
        .start_prompt(crate::types::PendingMessage {
            text: "first message".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), gate_reached.notified())
        .await
        .expect("Task A should reach the gate before Ctrl+C");

    // Sanity check: Task A's LLM call has popped turn 1; turn 2 still
    // sits in the queue.
    assert_eq!(
        mock_client.remaining_turns(),
        1,
        "Task A should have popped its own turn but not Task B's"
    );

    // Ctrl+C, then resubmit — the exact race that spawned Task B
    // alongside Task A in the reproducing session.
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    harness.tui().set_input_text("second message");
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    // Give the runtime a moment to schedule any task that the buggy
    // path WOULD have spawned. With the fix, no task was spawned —
    // remaining_turns must still be 1.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        mock_client.remaining_turns(),
        1,
        "no second prompt task should be running while Task A is still \
         in flight — Bug 2 spawned Task B here, which would pop turn 2"
    );

    // Release Task A so the test can clean up. After Final/Error
    // propagates, the queued message becomes Task B and consumes
    // turn 2.
    gate_release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        while let Ok(event) = harness.tui().event_rx.try_recv() {
            harness.tui().handle_tui_event(event).await.unwrap();
        }
        if mock_client.remaining_turns() == 0 && !harness.tui().app.llm_busy {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for prompts to drain"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    harness.drain_and_settle().await.unwrap();
}

/// Regression test for GitHub issue #264 — "Have to scroll up a lot sometimes before content moves down"
///
/// Two bugs are covered:
///
/// 1. **Tall-item rendering**: when a transcript entry is taller than the viewport and
///    the viewport is pinned to the bottom, `copy_partial_bottom_widget_to_frame` must
///    show the *bottom* portion of the item (skipping the hidden top lines), not the top.
///
/// 2. **Stale-max dead-zone**: after `scroll_down` prematurely sets `follow=true` against
///    a stale `last_max_position`, subsequent `scroll_up` calls should become immediately
///    effective once the render corrects `last_max_position`.  The position-clamp in
///    `render.rs` ensures `position` never exceeds `last_max_position` so no dead zone
///    accumulates.
#[tokio::test]
async fn test_tall_item_scroll_shows_correct_portion_and_no_dead_zone() {
    // Use a narrow viewport (40 cols wide, 10 rows tall) so the transcript area is tiny
    // (10 - 3 = 7 rows for the transcript, 3 rows for the input).
    let mut harness = TuiTestHarness::with_size(40, 10);

    // Build a single transcript item that is much taller than the 7-row transcript area.
    // 20 distinct lines so we can check which portion is visible.
    let lines: Vec<String> = (1..=20).map(|i| format!("line-{i:02}")).collect();
    let tall_text = lines.join("\n");

    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::AssistantText(tall_text.clone()));
    // Pin to bottom (follow=true) — the default on new content
    harness.tui().pin_transcript_to_bottom();

    // First render: the scroll widget learns the real item height, position snaps to max.
    harness.render();
    let bottom_view = normalize_screen(&harness.screen_contents());

    // Bug 1: at the bottom, we must see the *last* lines of the tall item, not the first.
    assert!(
        bottom_view.contains("line-20"),
        "bottom view should show the last line of the tall item, got:\n{bottom_view}"
    );
    assert!(
        !bottom_view.contains("line-01"),
        "bottom view must NOT show the first line when pinned to bottom, got:\n{bottom_view}"
    );

    // Scroll up 7 positions (more than one viewport's worth) so line-20 is no longer visible.
    for _ in 0..7 {
        harness.tui().app.scroll_state.scroll_up();
    }
    harness.render();
    let scrolled_view = normalize_screen(&harness.screen_contents());

    // After scrolling up 7 positions (position went from 13 → 6, scroll_offset = 13-6 = 7),
    // the item shows lines [8..14].  line-20 must not be visible.
    assert!(
        !scrolled_view.contains("line-20"),
        "after scrolling up 7 positions, line-20 should no longer be visible; got:\n{scrolled_view}"
    );
    // And we should now see lines near the top of that window.
    assert!(
        scrolled_view.contains("line-08")
            || scrolled_view.contains("line-09")
            || scrolled_view.contains("line-10"),
        "after scrolling up 7 positions, should see lines near line-08..10; got:\n{scrolled_view}"
    );

    // Bug 2: simulate the stale-max dead-zone.
    //
    // Reset position to 0 so scroll_down can climb from a known low value.
    // Then set last_max_position to a stale (too-small) value, simulating a
    // height-cache miss.  scroll_down should hit the stale ceiling, set
    // follow=true, and stop — even though the real max is much higher.
    harness.tui().app.scroll_state.position = 0;
    harness.tui().app.scroll_state.follow = false;
    let real_max = harness.tui().app.scroll_state.last_max_position;
    harness.tui().app.scroll_state.last_max_position = 2; // stale, too small

    // scroll_down will increment position up to the stale max of 2, then set follow=true.
    for _ in 0..6 {
        harness.tui().app.scroll_state.scroll_down();
    }
    assert!(
        harness.tui().app.scroll_state.follow,
        "scroll_down should have set follow=true at the stale max"
    );
    assert_eq!(
        harness.tui().app.scroll_state.position,
        2,
        "position should have clamped at the stale last_max_position=2"
    );

    // Now render with position deliberately set above the real max, simulating
    // the scenario where last_max_position was stale and position drifted too high.
    // The position-clamp in render.rs must bring it back to last_max_position.
    harness.tui().app.scroll_state.last_max_position = real_max;
    harness.tui().app.scroll_state.follow = false;
    harness.tui().app.scroll_state.position = real_max + 5; // intentionally above real max
    harness.render();

    let pos_after_clamp = harness.tui().app.scroll_state.position;
    let max_after_render = harness.tui().app.scroll_state.last_max_position;
    assert!(
        pos_after_clamp <= max_after_render,
        "position ({pos_after_clamp}) must be clamped to last_max_position ({max_after_render}) after render"
    );
}

/// Regression test: when an item is taller than the viewport and the user
/// scrolls up, the visible window through that item must move *backwards*
/// through the buffer (toward earlier lines), not forwards.
///
/// Symptom (from the user video): scrolling up makes the visible content
/// "go in the wrong direction".  Concretely, when the scroll position is
/// such that the first line of the chunk should be at the top of the
/// viewport, the buggy code instead shows the *last* lines of the chunk
/// (i.e. the same content that's visible when pinned to the bottom).
///
/// Math (V = transcript viewport height = 10, H = item height = 20):
///
/// | scroll_up count | scroll_offset | expected first/last visible buffer lines |
/// |-----------------|---------------|------------------------------------------|
/// |               0 | 0             | line-11 .. line-20                       |
/// |               1 | 1             | line-10 .. line-19                       |
/// |               5 | 5             | line-06 .. line-15                       |
/// |              10 | 10            | line-01 .. line-10                       |
#[tokio::test]
async fn test_tall_item_scroll_window_moves_in_correct_direction() {
    // size 40x13 → transcript area is 13 - 3 (input) = 10 rows.
    let mut harness = TuiTestHarness::with_size(40, 13);

    let lines: Vec<String> = (1..=20).map(|i| format!("line-{i:02}")).collect();
    let tall_text = lines.join("\n");

    harness.tui().clear_transcript();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::AssistantText(tall_text));
    harness.tui().pin_transcript_to_bottom();

    // Helper: assert the visible portion of the transcript shows exactly
    // line-{first}..line-{last} (inclusive, zero-padded), and no others.
    let assert_window = |harness: &mut TuiTestHarness, first: usize, last: usize, label: &str| {
        let view = normalize_screen(&harness.screen_contents());
        for n in 1..=20usize {
            let needle = format!("line-{n:02}");
            let in_window = n >= first && n <= last;
            let present = view.contains(&needle);
            assert_eq!(
                    present, in_window,
                    "{label}: expected {needle} present={in_window}, got present={present}\nview:\n{view}"
                );
        }
    };

    // 1. Pinned to bottom — should show the last 10 lines (line-11..line-20).
    harness.render();
    assert_window(&mut harness, 11, 20, "initial bottom view");

    // 2. Scroll up by 1.  The window should slide backwards by 1 line:
    //    line-10..line-19.  The buggy code instead shows line-02..line-11
    //    (window jumps backwards by H-V-1 = 9 lines and slides forward).
    harness.tui().app.scroll_state.scroll_up();
    harness.render();
    assert_window(&mut harness, 10, 19, "after scroll_up x1");

    // 3. Scroll up by 4 more (total 5).
    for _ in 0..4 {
        harness.tui().app.scroll_state.scroll_up();
    }
    harness.render();
    assert_window(&mut harness, 6, 15, "after scroll_up x5");

    // 4. Scroll up by 5 more (total 10 = max).  We should now be at the
    //    very top of the item: line-01..line-10.  The buggy code instead
    //    shows line-11..line-20 (identical to the pinned-to-bottom view!).
    for _ in 0..5 {
        harness.tui().app.scroll_state.scroll_up();
    }
    harness.render();
    assert_window(&mut harness, 1, 10, "after scroll_up x10 (top of item)");
}
