//! `c` (copy) and `i` (insert) action cases, clipboard seam tests, and the
//! no-focus degradation of the browse action keys.

use super::*;

fn copy_insert_cases() -> Vec<BrowseActionCase> {
    vec![
        BrowseActionCase::new(
            "copy on a row without copyable text",
            vec![RowSpec::SubAgent],
            0,
            'c',
        )
        .notice("Nothing to copy"),
        BrowseActionCase::new(
            "insert on a row without copyable text",
            vec![RowSpec::SubAgent],
            0,
            'i',
        )
        .notice("Nothing to insert"),
    ]
}

#[tokio::test]
async fn copy_insert_action_outcomes() {
    for case in copy_insert_cases() {
        assert_browse_action(&case).await;
    }
}

#[tokio::test]
async fn copy_with_text_shows_copied_notice() {
    let copied = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = copied.clone();
    let mut harness = browsing_harness(
        vec![RowSpec::User {
            text: "copy me",
            seq: Some(1),
        }],
        0,
        None,
    )
    .await;
    harness.tui().copy_to_clipboard_override = Some(std::sync::Arc::new(move |text: &str| {
        recorded.lock().unwrap().push(text.to_string());
        Ok(())
    }));

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(
        harness.tui().app.footer_notice_text(),
        Some("✓ Copied to clipboard")
    );
    assert_eq!(*copied.lock().unwrap(), vec!["copy me".to_string()]);
}

#[tokio::test]
async fn copy_failure_shows_notice() {
    // The clipboard write can fail (no clipboard provider, full buffer); the
    // user must see that instead of a false success notice (CodeRabbit
    // review of PR #2622).
    let mut harness = browsing_harness(
        vec![RowSpec::User {
            text: "copy me",
            seq: Some(1),
        }],
        0,
        None,
    )
    .await;
    harness.tui().copy_to_clipboard_override = Some(std::sync::Arc::new(|_| {
        Err(anyhow::anyhow!("clipboard unavailable"))
    }));

    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(harness.tui().app.footer_notice_text(), Some("Copy failed"));
}

#[tokio::test]
async fn browse_actions_without_focus_show_notice_instead_of_panicking() {
    // A shared-session refresh can clear transcript_focus while
    // transcript_browsing stays on; the action keys must degrade to a
    // notice instead of unwrapping a missing focus.
    let mut harness = TuiTestHarness::new().await;
    harness.tui().app.transcript.clear();
    harness.tui().app.transcript.push(row(&RowSpec::User {
        text: "Alpha",
        seq: Some(1),
    }));
    harness.tui().app.transcript_browsing = true;
    assert_eq!(harness.tui().app.transcript_focus, None);

    for key in ['r', 'e', 'i', 'c'] {
        harness
            .tui()
            .handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
            .await
            .unwrap();

        assert_eq!(
            harness.tui().app.footer_notice_text(),
            Some("Nothing selected"),
            "key '{key}' without focus must show a notice"
        );
        assert_eq!(harness.tui().app.modal, None, "key '{key}'");
    }

    // 'd' also degrades to a refusal notice rather than unwrapping.
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(
        harness.tui().app.footer_notice_text(),
        Some("Can't delete this item")
    );
    assert_eq!(harness.tui().app.modal, None);
}
