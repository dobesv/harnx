use super::*;

async fn press(tui: &mut Tui, code: KeyCode) {
    tui.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        .await
        .unwrap();
}

#[tokio::test]
async fn pending_attachments_navigate_between_draft_and_transcript() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript.push(TranscriptItem::UserText {
        text: "earlier".into(),
        seq: Some(1),
        timestamp: None,
    });
    tui.handle_paste("x".repeat(513)).await;
    tui.handle_paste("y".repeat(513)).await;
    tui.set_input_text("unsent draft");
    press(&mut tui, KeyCode::Up).await;
    assert_eq!(tui.app.pending_attachment_view.focus, Some(1));
    press(&mut tui, KeyCode::Up).await;
    assert_eq!(tui.app.pending_attachment_view.focus, Some(0));
    press(&mut tui, KeyCode::Up).await;
    assert!(tui.app.transcript_browsing);
    assert!(tui.app.pending_attachment_view.focus.is_none());
    press(&mut tui, KeyCode::Down).await;
    assert_eq!(tui.app.pending_attachment_view.focus, Some(0));
    press(&mut tui, KeyCode::Down).await;
    press(&mut tui, KeyCode::Down).await;
    assert!(tui.app.pending_attachment_view.focus.is_none());
    assert_eq!(tui.app.input.lines().join("\n"), "unsent draft");
    assert_eq!(tui.app.attachments.len(), 2);
    tui.cleanup_attachments();
}

#[tokio::test]
async fn pending_attachment_preview_removal_requires_confirmation() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.handle_paste("preview content\n".repeat(40)).await;
    let path = tui.app.attachments[0].path.clone();
    press(&mut tui, KeyCode::Up).await;
    press(&mut tui, KeyCode::Enter).await;
    assert!(tui.app.detail_view_open);
    assert!(tui
        .app
        .detail_view_text
        .as_ref()
        .unwrap()
        .contains("preview content"));
    press(&mut tui, KeyCode::Delete).await;
    assert!(tui.app.pending_attachment_view.confirming_delete);
    press(&mut tui, KeyCode::Enter).await;
    assert!(tui.app.detail_view_open);
    assert!(path.exists());
    press(&mut tui, KeyCode::Char('d')).await;
    press(&mut tui, KeyCode::Char('y')).await;
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.pending_attachment_view.focus.is_none());
    assert!(tui.app.attachments.is_empty());
    assert!(!path.exists());
}

#[tokio::test]
async fn confirmed_removal_keeps_same_name_unselected_attachment() {
    for selected in 0..2 {
        for preview in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let original = root.path().join("paste-1.txt");
            tokio::fs::write(&original, "explicit attachment")
                .await
                .unwrap();
            let config = test_config();
            let mut tui = Tui::init(&config).await.unwrap();
            tui.set_input_text(&format!(".attach {}", original.display()));
            press(&mut tui, KeyCode::Enter).await;
            tui.handle_paste("pasted attachment\n".repeat(40)).await;
            assert_eq!(tui.app.attachments.len(), 2);
            assert_eq!(
                tui.app.attachments[0].display_name,
                tui.app.attachments[1].display_name
            );
            let removed = tui.app.attachments[selected].path.clone();
            let retained = tui.app.attachments[1 - selected].path.clone();
            assert_ne!(removed, retained);
            let retained_bytes = tokio::fs::read(&retained).await.unwrap();
            tui.set_input_text("keep this draft");
            press(&mut tui, KeyCode::Up).await;
            if selected == 0 {
                press(&mut tui, KeyCode::Up).await;
            }
            if preview {
                press(&mut tui, KeyCode::Enter).await;
            }
            press(&mut tui, KeyCode::Delete).await;
            press(&mut tui, KeyCode::Char('y')).await;
            assert_eq!(tui.app.attachments.len(), 1);
            assert_eq!(tui.app.attachments[0].path, retained);
            assert_eq!(tokio::fs::read(&retained).await.unwrap(), retained_bytes);
            assert!(!removed.exists());
            assert!(!tui.app.detail_view_open);
            assert_eq!(tui.app.pending_attachment_view.focus, Some(0));
            assert_eq!(tui.app.input.lines().join("\n"), "keep this draft");
            assert_eq!(
                tokio::fs::read_to_string(&original).await.unwrap(),
                "explicit attachment"
            );
            tui.cleanup_attachments();
        }
    }
}

#[tokio::test]
async fn page_keys_scroll_pending_preview_without_moving_transcript() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.handle_paste("preview line\n".repeat(100)).await;
    press(&mut tui, KeyCode::Up).await;
    press(&mut tui, KeyCode::Enter).await;
    tui.app.scroll_state.position = 12;
    tui.app.scroll_state.last_max_position = 100;
    tui.app.detail_view_scroll.position = 20;
    tui.app.detail_view_scroll.last_max_position = 100;
    press(&mut tui, KeyCode::PageDown).await;
    assert_eq!(tui.app.detail_view_scroll.position, 30);
    assert_eq!(tui.app.scroll_state.position, 12);
    press(&mut tui, KeyCode::PageUp).await;
    assert_eq!(tui.app.detail_view_scroll.position, 20);
    assert_eq!(tui.app.scroll_state.position, 12);
    // The composer fallback preserves the same routing if reached directly.
    tui.scroll_input_page(false);
    assert_eq!(tui.app.detail_view_scroll.position, 30);
    assert_eq!(tui.app.scroll_state.position, 12);
    press(&mut tui, KeyCode::Esc).await;
    press(&mut tui, KeyCode::Esc).await;
    press(&mut tui, KeyCode::PageDown).await;
    assert_eq!(tui.app.scroll_state.position, 22);
    assert_eq!(tui.app.detail_view_scroll.position, 30);
    press(&mut tui, KeyCode::PageUp).await;
    assert_eq!(tui.app.scroll_state.position, 12);
    tui.cleanup_attachments();
}

#[tokio::test]
async fn pending_attachment_list_is_above_input_and_scrolls_with_selection() {
    let mut harness = TuiTestHarness::with_size(80, 18).await;
    for index in 0..9 {
        harness
            .tui()
            .app
            .attachments
            .push(crate::types::Attachment {
                path: format!("/tmp/file-{index}.txt").into(),
                display_name: format!("file-{index}.txt"),
            });
    }
    harness
        .tui()
        .set_input_text("draft stays below attachments");
    harness.tui().focus_pending_attachment(0);
    harness.render();
    let screen = harness.screen_contents();
    assert!(screen.find("file-0.txt").unwrap() < screen.find("draft stays").unwrap());
    harness.tui().focus_pending_attachment(8);
    harness.render();
    assert!(harness.screen_contents().contains("file-8.txt"));
}

#[cfg(unix)]
#[tokio::test]
async fn attachment_picker_hotkey_restores_terminal_hooks_and_keeps_original_file() {
    let _lock = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("file with spaces.txt");
    tokio::fs::write(&source, "picked content").await.unwrap();
    let config = test_config();
    let suspended = Arc::new(AtomicBool::new(false));
    let resumed = Arc::new(AtomicBool::new(false));
    let before = suspended.clone();
    let after = resumed.clone();
    config.write().set_tui_editor_hooks(
        Some(Box::new(move || before.store(true, Ordering::SeqCst))),
        Some(Box::new(move || after.store(true, Ordering::SeqCst))),
    );
    let mut tui = Tui::init(&config).await.unwrap();
    tui.set_input_text("draft before picking");
    pick_command(
        &mut tui,
        &format!(
            "printf '%s\\n' {}",
            shell_words::quote(source.to_str().unwrap())
        ),
    )
    .await;
    assert!(suspended.load(Ordering::SeqCst));
    assert!(resumed.load(Ordering::SeqCst));
    assert_eq!(tui.app.attachments[0].display_name, "file with spaces.txt");
    assert_ne!(tui.app.attachments[0].path, source);
    assert_eq!(tui.app.input.lines().join("\n"), "draft before picking");
    tui.cleanup_attachments();
    assert_eq!(
        tokio::fs::read_to_string(source).await.unwrap(),
        "picked content"
    );
}

#[cfg(unix)]
async fn pick_command(tui: &mut Tui, command: &str) {
    let prior = std::env::var_os("HARNX_ATTACHMENT_PICKER");
    std::env::set_var("HARNX_ATTACHMENT_PICKER", command);
    let result = tui
        .handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .await;
    match prior {
        Some(value) => std::env::set_var("HARNX_ATTACHMENT_PICKER", value),
        None => std::env::remove_var("HARNX_ATTACHMENT_PICKER"),
    }
    result.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn failed_or_cancelled_picker_preserves_a_queued_message() {
    let _lock = ENV_LOCK.lock().await;
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.set_input_text("already queued");
    let pending = crate::types::PendingMessage {
        text: "already queued".into(),
        attachments: vec![],
        attachment_dir: None,
        paste_count: 0,
    };
    tui.app.pending_message = Some(pending.clone());
    *tui.shared_pending_message.lock().await = Some(pending);
    pick_command(&mut tui, "printf '%s\\n' /nonexistent/selected-file.txt").await;
    assert!(tui.app.attachments.is_empty());
    assert_eq!(
        tui.app.pending_message.as_ref().unwrap().text,
        "already queued"
    );
    assert_eq!(
        tui.shared_pending_message
            .lock()
            .await
            .as_ref()
            .unwrap()
            .text,
        "already queued"
    );
    let directory = tempfile::tempdir().unwrap();
    pick_command(
        &mut tui,
        &format!(
            "printf '%s\\n' {}",
            shell_words::quote(directory.path().to_str().unwrap())
        ),
    )
    .await;
    assert!(
        tui.app.attachment_dir.is_none(),
        "A failed copy must roll back its new temp directory"
    );
    assert!(
        directory.path().exists(),
        "The selected source directory is untouched"
    );
    pick_command(&mut tui, "exit 130").await;
    assert!(tui.app.pending_message.is_some());
    assert!(tui.shared_pending_message.lock().await.is_some());
    assert_eq!(tui.app.input.lines().join("\n"), "already queued");
}

#[tokio::test]
async fn queued_submission_closes_pending_preview_and_confirmation() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.handle_paste("queued file\n".repeat(50)).await;
    press(&mut tui, KeyCode::Up).await;
    press(&mut tui, KeyCode::Enter).await;
    press(&mut tui, KeyCode::Char('d')).await;
    let pending = crate::types::PendingMessage {
        text: ".echo delivered".into(),
        attachments: tui.app.attachments.clone(),
        attachment_dir: tui.app.attachment_dir.clone(),
        paste_count: 1,
    };
    tui.submit_pending_message_inner(pending).await.unwrap();
    assert!(tui.app.pending_attachment_view.focus.is_none());
    assert!(!tui.app.pending_attachment_view.confirming_delete);
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.detail_view_text.is_none());
    tui.cleanup_attachments();
}

#[tokio::test]
async fn stale_pending_attachment_key_cannot_leak_into_a_draft() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.handle_paste("x".repeat(513)).await;
    press(&mut tui, KeyCode::Up).await;
    press(&mut tui, KeyCode::Enter).await;
    tui.app.attachments.clear();
    press(&mut tui, KeyCode::Char('y')).await;
    assert_eq!(tui.app.input.lines().join("\n"), "");
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.pending_attachment_view.focus.is_none());
    tui.cleanup_attachments();
}
