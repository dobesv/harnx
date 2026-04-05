use super::*;
use crate::config::Config;
use crate::tui::types::{TranscriptEntry, TuiEvent};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::Mutex;

fn test_config() -> GlobalConfig {
    Arc::new(RwLock::new(Config::default()))
}

#[tokio::test]
async fn pending_message_is_cleared_when_user_edits_again() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("queued message".to_string());

    tui.apply_draft_edit_for_test(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

    assert!(tui.app.pending_message.is_none());
    assert_eq!(tui.app.input.lines().join("\n"), "x");
}

#[tokio::test]
async fn pending_message_is_auto_sent_after_finish() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("follow up".to_string());

    tui.handle_tui_event(TuiEvent::Finished {
        output: "done".to_string(),
        usage: Default::default(),
        tool_results: vec![],
    })
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
async fn streaming_chunks_accumulate_across_interleaved_ui_output() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    tui.handle_tui_event(TuiEvent::Chunk("Hello\nworld".to_string()))
        .await
        .unwrap();
    tui.handle_tui_event(TuiEvent::UiOutput("tool output".to_string()))
        .await
        .unwrap();
    tui.handle_tui_event(TuiEvent::Chunk("\nAgain".to_string()))
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
async fn compute_completions_handles_trailing_space_after_command() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let line = ".model ";
    let completions = tui.compute_completions(line, line.len());

    assert!(completions.iter().all(|(value, _)| !value.is_empty()));
}

#[tokio::test]
async fn compute_completions_appends_space_for_command_matches() {
    let config = test_config();
    let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
    let tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

    let completions = tui.compute_completions(".mod", 4);

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
