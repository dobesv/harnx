use crate::test_utils::TuiTestHarness;
use crate::tests::make_tui_with_history;
use crate::types::TranscriptItem;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

async fn browsing_harness_with_items(count: usize, height: u16) -> TuiTestHarness {
    let mut harness = TuiTestHarness::with_size(60, height).await;
    harness.tui().clear_transcript();
    let items: Vec<TranscriptItem> = (0..count)
        .map(|i| TranscriptItem::UserText {
            text: format!("Browsing item {i:02}"),
            seq: None,
            timestamp: None,
        })
        .collect();
    harness.tui().app.transcript.extend(items);
    harness.render();
    harness
}

#[tokio::test]
async fn transcript_esc_with_nonempty_draft_enters_browsing_and_returns_intact() {
    let mut harness = TuiTestHarness::with_size(60, 16).await;
    harness.tui().clear_transcript();
    harness.tui().app.transcript.push(TranscriptItem::UserText {
        text: "First prompt".to_string(),
        seq: Some(1),
        timestamp: None,
    });
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::AssistantText {
            text: "Assistant reply".to_string(),
            seq: Some(2),
            timestamp: None,
            rendered_cache: None,
        });
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::SystemText("trailing notice".to_string()));
    harness.tui().set_input_text("line one\nline two");

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(harness.tui().app.transcript_browsing);
    assert_eq!(harness.tui().app.transcript_focus, Some(1));
    assert_eq!(harness.tui().app.transcript_selection_anchor, None);
    assert!(!harness.tui().app.scroll_state.follow);
    assert_eq!(
        harness.tui().app.input.lines().join("\n"),
        "line one\nline two"
    );

    harness.render();
    let browse_screen = harness.screen_contents();
    assert!(
        browse_screen.contains("Item 2 of 2"),
        "browse view should focus the last navigable item, got:\n{browse_screen}"
    );

    // Pasting while browsing via Esc must not mutate the hidden composer draft.
    harness.tui().handle_paste("hidden paste".to_string()).await;
    assert_eq!(
        harness.tui().app.input.lines().join("\n"),
        "line one\nline two"
    );

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(!harness.tui().app.transcript_browsing);
    assert_eq!(harness.tui().app.transcript_focus, None);
    assert!(harness.tui().app.scroll_state.follow);
    assert_eq!(
        harness.tui().app.input.lines().join("\n"),
        "line one\nline two"
    );
    harness.render();
    let composer_screen = harness.screen_contents();
    assert!(
        composer_screen.contains("line one") && composer_screen.contains("line two"),
        "composer should restore draft visibly, got:\n{composer_screen}"
    );
}

#[tokio::test]
async fn transcript_esc_respects_completions_and_inline_focus_precedence() {
    let mut harness = TuiTestHarness::with_size(60, 16).await;
    harness.tui().clear_transcript();
    harness.tui().app.transcript.push(TranscriptItem::UserText {
        text: "Prompt".to_string(),
        seq: Some(1),
        timestamp: None,
    });
    harness.tui().set_input_text(".ed");
    harness.tui().app.completions = vec![(".edit ".to_string(), None)];

    // 1st Esc dismisses completions without entering browse.
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(harness.tui().app.completions.is_empty());
    assert!(!harness.tui().app.transcript_browsing);
    assert_eq!(harness.tui().app.transcript_focus, None);
    assert_eq!(harness.tui().app.input.lines().join("\n"), ".ed");

    // 2nd Esc enters browse, preserving draft.
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(harness.tui().app.transcript_browsing);
    assert_eq!(harness.tui().app.transcript_focus, Some(0));
    assert_eq!(harness.tui().app.input.lines().join("\n"), ".ed");

    // Exit browse and test inline focus precedence (Shift+Up leaves browsing=false).
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!harness.tui().app.transcript_browsing);
    harness.tui().set_input_text("");
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT))
        .await
        .unwrap();
    assert_eq!(harness.tui().app.transcript_focus, Some(0));
    assert!(!harness.tui().app.transcript_browsing);

    // 1st Esc clears inline focus without entering fullscreen browse.
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(harness.tui().app.transcript_focus, None);
    assert!(!harness.tui().app.transcript_browsing);

    // 2nd Esc enters fullscreen browse.
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(harness.tui().app.transcript_browsing);
    assert_eq!(harness.tui().app.transcript_focus, Some(0));
}

#[tokio::test]
async fn transcript_esc_without_navigable_target_shows_notice_without_invalid_focus() {
    for with_system_only in [false, true] {
        let mut harness = TuiTestHarness::with_size(60, 16).await;
        harness.tui().clear_transcript();
        if with_system_only {
            harness
                .tui()
                .app
                .transcript
                .push(TranscriptItem::SystemText("Welcome banner".to_string()));
        }
        harness.tui().app.history = vec!["older submission".to_string()];
        harness.tui().set_input_text("draft stays");

        harness
            .tui()
            .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();

        assert!(!harness.tui().app.transcript_browsing);
        assert_eq!(harness.tui().app.transcript_focus, None);
        assert_eq!(harness.tui().app.transcript_selection_anchor, None);
        assert!(!harness.tui().app.history_preview);
        assert_eq!(harness.tui().app.input.lines().join("\n"), "draft stays");
        assert_eq!(
            harness.tui().app.footer_notice_text(),
            Some("Nothing to browse")
        );

        harness.render();
        let screen = harness.screen_contents();
        assert!(
            screen.contains("Nothing to browse"),
            "no-target Esc notice must render visibly, got:\n{screen}"
        );

        // Typing afterward clears the notice and edits the draft normally.
        harness
            .tui()
            .handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(harness.tui().app.footer_notice_text(), None);
        assert_eq!(harness.tui().app.input.lines().join("\n"), "draft stays!");
    }
}

#[tokio::test]
async fn transcript_esc_preserves_history_preview_state() {
    let (mut tui, _config) = make_tui_with_history(&["first entry", "second entry"]).await;
    tui.app.transcript.clear();
    tui.app.transcript.push(TranscriptItem::UserText {
        text: "Prompt".to_string(),
        seq: Some(1),
        timestamp: None,
    });
    tui.set_input_text("unsent");
    tui.app.history_draft = "unsent".to_string();
    tui.app.history_index = Some(0);
    tui.app.history_preview = true;
    tui.set_input_text("first entry");

    tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.app.transcript_browsing);
    assert_eq!(tui.app.input.lines().join("\n"), "first entry");
    assert!(tui.app.history_preview);
    assert_eq!(tui.app.history_index, Some(0));
    assert_eq!(tui.app.history_draft, "unsent");

    tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.transcript_browsing);
    assert_eq!(tui.app.input.lines().join("\n"), "first entry");
    assert!(tui.app.history_preview);
    assert_eq!(tui.app.history_index, Some(0));
    assert_eq!(tui.app.history_draft, "unsent");
}

#[tokio::test]
async fn test_browsing_mode_page_up_and_page_down_scroll() {
    let mut harness = browsing_harness_with_items(25, 12).await;

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(harness.tui().app.transcript_browsing);
    let initial_focus = harness.tui().app.transcript_focus;

    harness.render();
    let max_pos = harness.tui().app.browsing_view_scroll.last_max_position;
    assert!(max_pos > 10, "browsing view needs >10 scrollable lines");
    let start_pos = harness.tui().app.browsing_view_scroll.position;
    assert!(start_pos >= 10, "initial focus should start near bottom");

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        harness.tui().app.browsing_view_scroll.position,
        start_pos - 10,
        "PageUp in browse mode should scroll up 10 lines"
    );
    assert!(harness.tui().app.transcript_browsing);
    assert_eq!(harness.tui().app.transcript_focus, initial_focus);

    let after_up = harness.tui().app.browsing_view_scroll.position;
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        harness.tui().app.browsing_view_scroll.position,
        (after_up + 10).min(max_pos),
        "PageDown in browse mode should scroll down 10 lines"
    );
    assert!(harness.tui().app.transcript_browsing);
    assert_eq!(harness.tui().app.transcript_focus, initial_focus);
}
