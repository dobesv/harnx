//! Footer-notice lifecycle: expiry, clearing on navigation, and rendering in
//! the browse and detail footers.

use super::*;

#[test]
fn footer_notice_expires_after_deadline() {
    let mut app = crate::types::App::default();
    app.show_footer_notice("still visible");
    assert_eq!(app.footer_notice_text(), Some("still visible"));
    // Force the deadline into the past.
    app.footer_notice = Some(crate::types::FooterNotice {
        text: "stale".to_string(),
        until: std::time::Instant::now() - std::time::Duration::from_secs(1),
    });
    assert_eq!(
        app.footer_notice_text(),
        None,
        "an expired notice must not hide the hint legend"
    );
}

#[test]
fn detail_view_footer_text_shows_notice() {
    let mut app = crate::types::App::default();
    app.show_footer_notice("Can't rewind to this item");
    assert_eq!(
        crate::detail_view::detail_view_footer_text(&app),
        " Can't rewind to this item"
    );
}

#[tokio::test]
async fn navigation_keys_clear_footer_notice() {
    // Every navigation surface (browse focus moves, browse scroll, detail
    // scroll) must clear a stale notice so the hint legend returns
    // (CodeRabbit review of PR #2622 flagged the scroll keys).
    let browse_keys = [
        (KeyCode::Up, KeyModifiers::NONE),
        (KeyCode::Esc, KeyModifiers::NONE),
        (KeyCode::Char('g'), KeyModifiers::NONE),
        (KeyCode::Char('G'), KeyModifiers::NONE),
        (KeyCode::Home, KeyModifiers::NONE),
        (KeyCode::End, KeyModifiers::NONE),
    ];
    let detail_keys = [
        (KeyCode::Up, KeyModifiers::NONE),
        (KeyCode::Down, KeyModifiers::NONE),
        (KeyCode::PageUp, KeyModifiers::NONE),
        (KeyCode::PageDown, KeyModifiers::NONE),
        (KeyCode::Char('g'), KeyModifiers::NONE),
        (KeyCode::Char('G'), KeyModifiers::NONE),
    ];

    for (code, modifiers) in browse_keys {
        let mut harness = browsing_harness(
            vec![
                RowSpec::User {
                    text: "One",
                    seq: Some(1),
                },
                RowSpec::User {
                    text: "Two",
                    seq: Some(2),
                },
            ],
            1,
            None,
        )
        .await;
        harness.tui().app.show_footer_notice("stale notice");

        harness
            .tui()
            .handle_key(KeyEvent::new(code, modifiers))
            .await
            .unwrap();

        assert_eq!(
            harness.tui().app.footer_notice_text(),
            None,
            "browse key {code:?} must clear the footer notice so hints return"
        );
    }

    for (code, modifiers) in detail_keys {
        let mut harness = TuiTestHarness::new().await;
        harness.tui().app.detail_view_text = Some("detail body".to_string());
        harness.tui().app.detail_view_open = true;
        harness.tui().app.show_footer_notice("stale notice");

        harness
            .tui()
            .handle_key(KeyEvent::new(code, modifiers))
            .await
            .unwrap();

        assert_eq!(
            harness.tui().app.footer_notice_text(),
            None,
            "detail scroll key {code:?} must clear the footer notice"
        );
    }
}

#[tokio::test]
async fn test_browsing_footer_renders_notice_instead_of_hints() {
    let mut harness = TuiTestHarness::with_size(60, 20).await;
    harness.tui().app.transcript.clear();
    harness.tui().app.transcript.push(row(&RowSpec::User {
        text: "Alpha",
        seq: Some(1),
    }));
    harness.tui().app.transcript_browsing = true;
    harness.tui().app.transcript_focus = Some(0);
    harness
        .tui()
        .app
        .show_footer_notice("Can't delete this item");

    harness.render();
    let screen = harness.screen_contents();

    assert!(
        screen.contains("Can't delete this item"),
        "browse footer should show the notice, got:\n{screen}"
    );
    assert!(
        !screen.contains("e/edit"),
        "browse footer hints should be replaced while the notice is active, got:\n{screen}"
    );
}
