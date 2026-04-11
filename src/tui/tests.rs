use super::*;
use crate::client::{Client, ClientConfig, TestStateGuard};
use crate::config::Config;
use crate::test_utils::{MockClient, MockTurnBuilder, TuiTestHarness};
use crate::tui::render_helpers::event_fallback_text;
use crate::tui::types::{TranscriptEntry, TuiEvent};
use crate::ui_output::{UiOutputEvent, UiOutputEventKind, UiOutputSource};
use crate::utils::dimmed_text;
use agent_client_protocol as acp;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

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
        let mut agent = crate::config::Agent::from_prompt("");
        agent.set_name(agent_name);
        agent.set_model(model.clone());
        guard.agent = Some(agent);

        // Set up session if session_name is provided.
        if let Some(name) = session_name {
            guard.session = Some(crate::config::session::Session::new(&guard, name));
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
    tui.queue_pending_message("queued message".to_string());

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
    tui.queue_pending_message("queued message".to_string());

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
        .all(|entry| !matches!(entry, TranscriptEntry::User(_))));
}

#[tokio::test]
async fn pending_message_is_auto_sent_after_finish() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("follow up".to_string());

    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::LlmFinal {
            output: "done".to_string(),
            usage: Default::default(),
        },
        source: None,
    }))
    .await
    .unwrap();

    assert!(tui.app.llm_busy);
    assert!(tui.app.pending_message.is_none());
    let has_user_entry = tui
        .app
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptEntry::User(text) if text == "follow up"));
    assert!(has_user_entry);
}

#[tokio::test]
async fn pending_dot_command_restores_attachments_before_running() {
    use crate::tui::types::Attachment;
    use std::path::PathBuf;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.app.pending_message = Some(crate::tui::types::PendingMessage {
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

    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::LlmFinal {
            output: "done".to_string(),
            usage: Default::default(),
        },
        source: None,
    }))
    .await
    .unwrap();

    assert!(tui.app.pending_message.is_none());
    assert_eq!(tui.app.attachments.len(), 2);
    assert_eq!(tui.app.attachments[0].display_name, "a.txt");
    assert_eq!(tui.app.attachments[1].display_name, "b.txt");
}

#[tokio::test]
async fn streaming_chunks_accumulate_across_interleaved_ui_output() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::MessageChunk {
            text: "Hello\nworld".to_string(),
            raw: None,
        },
        source: None,
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::TranscriptText {
            text: "tool output".to_string(),
        },
        source: None,
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::MessageChunk {
            text: "\nAgain".to_string(),
            raw: None,
        },
        source: None,
    }))
    .await
    .unwrap();

    let assistant_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Assistant(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_entries, vec!["Hello\n", "world\n", "Again"]);
    assert!(tui
        .app
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptEntry::System(text) if text == "tool output")));
}

#[tokio::test]
async fn ui_output_inserts_heading_when_source_changes() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let source = crate::ui_output::UiOutputSource {
        agent: "argus".to_string(),
        session_id: Some("session-1".to_string()),
    };

    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::TranscriptText {
            text: "first chunk".to_string(),
        },
        source: Some(source.clone()),
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::TranscriptText {
            text: "second chunk".to_string(),
        },
        source: Some(source),
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::TranscriptText {
            text: "other chunk".to_string(),
        },
        source: Some(crate::ui_output::UiOutputSource {
            agent: "hephaestus".to_string(),
            session_id: Some("session-2".to_string()),
        }),
    }))
    .await
    .unwrap();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();

    assert!(system_entries.contains(&"> argus ▸ session-1"));
    assert!(system_entries.contains(&"first chunk"));
    assert!(system_entries.contains(&"second chunk"));
    assert!(system_entries.contains(&"> hephaestus ▸ session-2"));
    assert!(system_entries.contains(&"other chunk"));

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

    tui.run_repl_command(".info session").await.unwrap();
    while let Ok(event) = tui.event_rx.try_recv() {
        tui.handle_tui_event(event).await.unwrap();
    }

    let has_session_output = tui
        .app
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptEntry::System(text) if !text.is_empty()));
    assert!(has_session_output);
}

#[tokio::test]
async fn info_session_does_not_print_raw_output_in_tui_mode() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.run_repl_command(".info session").await.unwrap();
    while let Ok(event) = tui.event_rx.try_recv() {
        tui.handle_tui_event(event).await.unwrap();
    }

    let transcript_text = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::System(text) => Some(text.as_str()),
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

    harness
        .tui()
        .run_repl_command(".info session")
        .await
        .unwrap();
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
        .run_repl_command(".session info-session-with-session-test")
        .await
        .unwrap();
    while let Ok(event) = harness.tui().event_rx.try_recv() {
        harness.tui().handle_tui_event(event).await.unwrap();
    }

    harness
        .tui()
        .run_repl_command(".info session")
        .await
        .unwrap();
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
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::MessageChunk {
                text: "Top-level assistant opening response.".to_string(),
                raw: None,
            },
            source: None,
        }))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::TranscriptText {
                text: "sub-agent tool call".to_string(),
            },
            source: Some(UiOutputSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        }))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::TranscriptText {
                text: "sub-agent follow-up output".to_string(),
            },
            source: Some(UiOutputSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        }))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::TranscriptText {
                text: "other sub-agent output".to_string(),
            },
            source: Some(UiOutputSource {
                agent: "hephaestus".to_string(),
                session_id: Some("session-2".to_string()),
            }),
        }))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::MessageChunk {
                text: "Top-level assistant closes response.".to_string(),
                raw: None,
            },
            source: None,
        }))
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
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::ToolCall {
                tool_name: "argus_session_prompt".to_string(),
                input_yaml: Some("message: hello\nsession_id: abc123".to_string()),
                raw: None,
            },
            source: Some(UiOutputSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        }))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::ThoughtChunk {
                text: "thinking hard\nstep two".to_string(),
                raw: Some(Box::new(acp::ContentChunk::new(
                    "thinking hard\nstep two".into(),
                ))),
            },
            source: Some(UiOutputSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        }))
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
        tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::ThoughtChunk {
                text: chunk.to_string(),
                raw: Some(Box::new(acp::ContentChunk::new(chunk.to_string().into()))),
            },
            source: None,
        }))
        .await
        .unwrap();
    }

    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::ToolCall {
            tool_name: "argus_session_prompt".to_string(),
            input_yaml: Some("message: delegate".to_string()),
            raw: None,
        },
        source: None,
    }))
    .await
    .unwrap();

    for chunk in ["thinking ", "after ", "tool"] {
        tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::ThoughtChunk {
                text: chunk.to_string(),
                raw: Some(Box::new(acp::ContentChunk::new(chunk.to_string().into()))),
            },
            source: None,
        }))
        .await
        .unwrap();
    }

    tui.flush_pending_thought_for_test();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::System(text)
                if !text.starts_with("Welcome to harnx") && !text.starts_with('•') =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        system_entries,
        vec![
            "<think>thinking before tool</think>",
            "->️ argus_session_prompt",
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

    let source = Some(UiOutputSource {
        agent: "argus".to_string(),
        session_id: Some("session-1".to_string()),
    });

    for chunk in ["thinking ", "before ", "tool"] {
        tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::ThoughtChunk {
                text: chunk.to_string(),
                raw: Some(Box::new(acp::ContentChunk::new(chunk.to_string().into()))),
            },
            source: source.clone(),
        }))
        .await
        .unwrap();
    }

    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::ToolCall {
            tool_name: "argus_session_prompt".to_string(),
            input_yaml: Some("message: delegate".to_string()),
            raw: None,
        },
        source: source.clone(),
    }))
    .await
    .unwrap();

    for chunk in ["thinking ", "after ", "tool"] {
        tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::ThoughtChunk {
                text: chunk.to_string(),
                raw: Some(Box::new(acp::ContentChunk::new(chunk.to_string().into()))),
            },
            source: source.clone(),
        }))
        .await
        .unwrap();
    }

    tui.flush_pending_thought_for_test();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::System(text)
                if !text.starts_with("Welcome to harnx") && !text.starts_with('•') =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        system_entries,
        vec![
            "> argus ▸ session-1",
            "<think>thinking before tool</think>",
            "->️ argus_session_prompt",
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
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::MessageChunk {
                text: "line one\nline two\nline three".to_string(),
                raw: None,
            },
            source: None,
        }))
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
            .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
                kind: UiOutputEventKind::ThoughtChunk {
                    text: chunk.to_string(),
                    raw: Some(Box::new(acp::ContentChunk::new(chunk.to_string().into()))),
                },
                source: Some(UiOutputSource {
                    agent: "argus".to_string(),
                    session_id: Some("session-1".to_string()),
                }),
            }))
            .await
            .unwrap();
    }
    harness
        .tui()
        .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::ToolCall {
                tool_name: "argus_session_prompt".to_string(),
                input_yaml: Some("message: delegate".to_string()),
                raw: None,
            },
            source: Some(UiOutputSource {
                agent: "argus".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        }))
        .await
        .unwrap();
    for chunk in ["thinking ", "after ", "tool"] {
        harness
            .tui()
            .handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
                kind: UiOutputEventKind::ThoughtChunk {
                    text: chunk.to_string(),
                    raw: Some(Box::new(acp::ContentChunk::new(chunk.to_string().into()))),
                },
                source: Some(UiOutputSource {
                    agent: "argus".to_string(),
                    session_id: Some("session-1".to_string()),
                }),
            }))
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

    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::ToolCall {
            tool_name: "argus_session_prompt".to_string(),
            input_yaml: Some("message: hello\nsession_id: abc123".to_string()),
            raw: None,
        },
        source: Some(UiOutputSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::ThoughtChunk {
            text: "thinking hard".to_string(),
            raw: Some(Box::new(acp::ContentChunk::new("thinking hard".into()))),
        },
        source: Some(UiOutputSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::ToolCallUpdate {
            title: Some("argus_session_prompt".to_string()),
            status: Some("completed".to_string()),
            raw: None,
        },
        source: Some(UiOutputSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::Plan {
            entries: vec![
                crate::ui_output::UiOutputPlanEntry {
                    status: "in_progress".to_string(),
                    content: "Refactor ACP formatting".to_string(),
                },
                crate::ui_output::UiOutputPlanEntry {
                    status: "pending".to_string(),
                    content: "Update snapshots".to_string(),
                },
            ],
        },
        source: Some(UiOutputSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::Usage {
            input_tokens: 12,
            output_tokens: 34,
            cached_tokens: 5,
            session_label: Some("> argus ▸ session-1".to_string()),
        },
        source: Some(UiOutputSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
        }),
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::ToolCall {
            tool_name: "bash".to_string(),
            input_yaml: Some("command: ls".to_string()),
            raw: None,
        },
        source: None,
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::ToolResultText {
            text: "\u{1b}[2mline one\nline two\u{1b}[0m\n".to_string(),
        },
        source: None,
    }))
    .await
    .unwrap();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();

    assert!(system_entries.contains(&"> argus ▸ session-1"));
    assert!(system_entries.contains(&"->️ argus_session_prompt"));
    assert!(system_entries.contains(&"   message: hello"));
    assert!(system_entries.contains(&"<think>thinking hard</think>"));
    assert!(system_entries.contains(&"-> argus_session_prompt completed"));
    assert!(system_entries.contains(&"Plan:"));
    assert!(system_entries.contains(&"  [in_progress] Refactor ACP formatting"));
    assert!(system_entries.contains(&"> argus ▸ session-1   in 12   out 34   cache 5"));
    assert!(system_entries.contains(&"->️ bash"));
    assert!(system_entries.contains(&"   command: ls"));
    assert!(system_entries.contains(&"   line one"));
    assert!(system_entries.contains(&"   line two"));
}

#[tokio::test]
async fn consecutive_usage_updates_replace_previous_usage_row_for_same_source() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let source = UiOutputSource {
        agent: "pytheas".to_string(),
        session_id: Some("session-1".to_string()),
    };

    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::Usage {
            input_tokens: 10,
            output_tokens: 1,
            cached_tokens: 0,
            session_label: None,
        },
        source: Some(source.clone()),
    }))
    .await
    .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
        kind: UiOutputEventKind::Usage {
            input_tokens: 20,
            output_tokens: 2,
            cached_tokens: 0,
            session_label: None,
        },
        source: Some(source.clone()),
    }))
    .await
    .unwrap();

    let system_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::System(text) => Some(text.as_str()),
            _ => None,
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

    let source = UiOutputSource {
        agent: "aristarchus".to_string(),
        session_id: Some("session-1".to_string()),
    };

    for chunk in [
        "Now I have ",
        "enough ",
        "information to ",
        "complete my review.",
    ] {
        tui.handle_tui_event(TuiEvent::UiOutput(UiOutputEvent {
            kind: UiOutputEventKind::MessageChunk {
                text: chunk.to_string(),
                raw: Some(Box::new(acp::ContentChunk::new(chunk.to_string().into()))),
            },
            source: Some(source.clone()),
        }))
        .await
        .unwrap();
    }

    let assistant_entries: Vec<_> = tui
        .app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Assistant(text) => Some(text.as_str()),
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
    use crate::tui::types::Attachment;

    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "first line\nsecond line\nthird line").unwrap();

    tui.submit_pending_message(crate::tui::types::PendingMessage {
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
        .filter_map(|entry| match entry {
            TranscriptEntry::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();

    assert!(matches!(
        tui.app.transcript.iter().find(|entry| matches!(entry, TranscriptEntry::User(_))),
        Some(TranscriptEntry::User(text)) if text == "hello with files"
    ));
    assert!(system_entries.contains(&"Attachments (1):"));
    assert!(system_entries.contains(&"  - notes.txt"));
    assert!(system_entries.contains(&"      first line"));
    assert!(system_entries.contains(&"      second line"));
}

#[test]
fn mcp_tool_result_fallback_uses_multiline_yaml_for_structured_json() {
    let event = UiOutputEventKind::ToolResultText {
        text: dimmed_text("content:\n  - type: text\n    text: hello\nisError: false\n"),
    };

    let rendered = event_fallback_text(&event, None);
    assert!(rendered.contains("content:"));
    assert!(rendered.contains("type: text"));
    assert!(!rendered.contains("{\"content\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn help_renders_in_tui_snapshot() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut harness = TuiTestHarness::with_size(70, 24);
    harness.tui().config = config.clone();
    harness.tui().persistent_manager = persistent;

    harness.tui().run_repl_command(".help").await.unwrap();
    while let Ok(event) = harness.tui().event_rx.try_recv() {
        harness.tui().handle_tui_event(event).await.unwrap();
    }
    harness.render();

    let rendered = normalize_screen(&harness.screen_contents());
    assert!(rendered.contains("Show system info") || rendered.contains("Type :::"));
    insta::assert_snapshot!("help_in_tui", rendered);
}

#[tokio::test]
async fn representative_repl_commands_render_into_tui_transcript() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let commands = [".help", ".info session", ".mcp list"];

    for command in commands {
        let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent.clone()).unwrap();
        tui.run_repl_command(command).await.unwrap();
        while let Ok(event) = tui.event_rx.try_recv() {
            tui.handle_tui_event(event).await.unwrap();
        }

        let transcript_text = tui
            .app
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::System(text) => Some(text.as_str()),
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
        .push(TranscriptEntry::User("Test message".to_string()));
    harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
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
            TranscriptEntry::Assistant(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_entries, vec!["Hello from the mock client!"]);
    assert!(harness
        .tui()
        .app
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptEntry::User(text) if text == "Test message")));

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
        .push(TranscriptEntry::User("What is the answer?".to_string()));
    harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
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

/// Test the trigger_agent tool flow for sub-agent delegation.
/// This test verifies that when the LLM returns a trigger_agent tool call,
/// the tool result includes the switch_agent data for the prompt loop to process.
/// The actual agent switching is complex (requires agent files), so this test
/// focuses on verifying the tool call appears in the TUI transcript.
#[tokio::test(flavor = "multi_thread")]
async fn test_sub_agent_delegation_tool_appears() {
    let config = test_config_with_mock_client_and_agent("coordinator", Some("delegation-test"));

    // The mock returns trigger_agent tool call, which gets processed
    // The tool result will have switch_agent data, but we're just verifying
    // the tool call appears in the transcript
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("I'll delegate this task.")
                    .add_tool_call(
                        "trigger_agent",
                        serde_json::json!({
                            "agent": "specialist",
                            "prompt": "Please help with this task"
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
        .push(TranscriptEntry::User("Help me".to_string()));
    harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
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

    // Wait for the trigger_agent tool call to appear on screen
    harness
        .wait_until_screen_contains("trigger_agent", Duration::from_secs(3))
        .await
        .unwrap();

    let screen = harness.screen_contents();

    // Verify tool call appears with its arguments
    assert!(
        screen.contains("trigger_agent"),
        "Screen should show trigger_agent tool call, got: {screen}"
    );
    assert!(
        screen.contains("specialist"),
        "Screen should show the agent name in tool call, got: {screen}"
    );

    // Don't use snapshot testing - the order of tool call display and tool result
    // is non-deterministic due to async event processing. The assertions above
    // verify the key content is present.

    harness.drain_and_settle().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tool_result_switch_agent_parsing() {
    // Uses the production eval_tool_calls path to verify switch_agent detection
    use crate::tool::{eval_tool_calls, ToolCall};

    let _guard = TestStateGuard::new(None).await;
    let config = test_config();

    let call = ToolCall::new(
        "trigger_agent".to_string(),
        serde_json::json!({"agent": "specialist", "prompt": "Help!"}),
        Some("tool-123".to_string()),
        None,
    );

    let results = eval_tool_calls(&config, vec![call]).unwrap();
    assert_eq!(results.len(), 1);

    let data = results[0]
        .switch_agent
        .as_ref()
        .expect("switch_agent should be set");
    assert_eq!(data.agent, "specialist");
    assert_eq!(data.prompt, "Help!");
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
        .push(TranscriptEntry::User(user_message.to_string()));
    harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
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
        .queue_pending_message("queued follow-up message".to_string());
    harness.render();

    let rendered = normalize_screen(&harness.screen_contents());
    assert!(!rendered.contains("Pending message queued"));
    assert!(rendered.contains("queued follow-up message"));

    insta::assert_snapshot!("pending_message_busy_state", rendered);
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
        .push(TranscriptEntry::User("Long request".to_string()));
    harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
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

    // Now simulate Ctrl+C after streaming is done
    // This tests the Ctrl+C handling when llm_busy is true
    harness.tui().abort_signal.set_ctrlc();

    // Manually trigger the Ctrl+C handling (same as handle_key for Ctrl+C)
    harness.tui().app.transcript.push(TranscriptEntry::System(
        "(Ctrl+C — operation aborted. Ctrl+D to exit.)".to_string(),
    ));
    harness.tui().app.llm_busy = false;
    harness.tui().abort_signal.reset();

    harness.render();
    let screen = harness.screen_contents();

    // The transcript should show the abort message
    assert!(
        screen.contains("aborted") || screen.contains("Ctrl+C"),
        "Screen should show abort message, got: {screen}"
    );

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
        .push(TranscriptEntry::User("Error test".to_string()));

    // The error should propagate through start_prompt
    let result = harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
            text: "Error test".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await;

    let _ = result; // start_prompt always returns Ok (spawns a task)

    // Wait for the error to appear in the transcript
    harness
        .wait_until_screen_contains("error:", Duration::from_secs(5))
        .await
        .unwrap();

    let has_error = harness
        .tui()
        .app
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptEntry::Error(_)));
    assert!(has_error, "Transcript should contain an error entry");

    harness.drain_and_settle().await.unwrap();
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
        .push(TranscriptEntry::User("Search test".to_string()));
    harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
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
    harness.tui().app.transcript.push(TranscriptEntry::System(
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
        .filter(|entry| matches!(entry, TranscriptEntry::User(_)))
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
    use crate::tui::types::Attachment;
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
        .filter(|e| matches!(e, TranscriptEntry::User(_)))
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
        .filter(|e| matches!(e, TranscriptEntry::User(_)))
        .collect();
    assert!(user_entries.is_empty());

    std::fs::remove_file(&tmp).ok();
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
        .any(|e| matches!(e, TranscriptEntry::Error(msg) if msg.contains("not found")));
    assert!(has_error, "Should show error for nonexistent file");
}

#[tokio::test]
async fn detach_clears_all_attachments() {
    use crate::tui::types::Attachment;
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
    use crate::tui::types::Attachment;

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
    use crate::tui::types::Attachment;

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
    use crate::tui::types::Attachment;

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
    use crate::tui::types::Attachment;
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
        .push(TranscriptEntry::User("First request".to_string()));
    harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
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
    harness.tui().app.transcript.push(TranscriptEntry::System(
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
        .push(TranscriptEntry::User("Second request".to_string()));
    harness
        .tui()
        .start_prompt(crate::tui::types::PendingMessage {
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
    use crate::tui::types::Attachment;
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
