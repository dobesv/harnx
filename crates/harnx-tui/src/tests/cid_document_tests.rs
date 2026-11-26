use super::{resolved_blob, TuiTestHarness};
use crate::types::{CidDocView, TranscriptItem};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;

const PLAN_URL: &str = "cid:plan:_/session1/readiness";
const TASK_T1_URL: &str = "cid:plan:_/session1/readiness/tasks/t1";
const NOTE_IMPL_URL: &str = "cid:plan:_/session1/readiness/notes/impl";

const REPRESENTATIVE_PLAN_FIXTURE: &str = "\
# Plan: Release Readiness

Summary paragraph explaining why spacing, nested lists, wrapped continuation text, and fullscreen margins improve plan readability across narrow and normal viewports.

## Scope

- [x] Audit section spacing across headings, paragraphs, and blocks
  - [ ] Verify nested wrapped continuation lines stay aligned under item content across narrow terminal widths
  - Tight sibling item without extra blank row

## Tasks

| Status | Task | Dependencies |
|---|---|---|
| closed | [T1](cid:plan:_/session1/readiness/tasks/t1) | - |
| in_progress | T2 | T1 |

## Verification

```sh
cargo nextest run -p harnx-tui

cargo xtask quality
```

## Notes

- [Implementation](cid:plan:_/session1/readiness/notes/impl) — Iris
- [External Spec](https://example.com/spec)

FINAL PLAN LINE";

fn show_doc(harness: &mut TuiTestHarness, url: &str, title: &str, text: &str) {
    let links = crate::markdown_render::extract_markdown_links(text);
    let focused_link = (!links.is_empty()).then_some(0);
    harness.tui().show_cid_document(CidDocView {
        url: url.into(),
        title: title.into(),
        text: text.into(),
        links,
        focused_link,
    });
    harness.render();
}

fn buffer_row(harness: &TuiTestHarness, y: u16) -> String {
    let buf = harness.terminal.backend().buffer();
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn trim_screen_lines(screen: &str) -> String {
    screen
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

async fn press_key(harness: &mut TuiTestHarness, code: KeyCode, modifiers: KeyModifiers) {
    harness
        .tui()
        .handle_key(KeyEvent::new(code, modifiers))
        .await
        .unwrap();
    harness.render();
}

#[tokio::test]
async fn cid_document_plan_fixture_snapshot_normal_80x24() {
    let mut harness = TuiTestHarness::with_size(80, 24).await;
    show_doc(
        &mut harness,
        PLAN_URL,
        "Plan: Release Readiness",
        REPRESENTATIVE_PLAN_FIXTURE,
    );

    let screen = harness.screen_contents();
    let trimmed = trim_screen_lines(&screen);
    insta::assert_snapshot!("cid_document_plan_fixture_80x24", trimmed);

    // Row 0 is the top border + title, row 1 is the blank top margin,
    // rows 2..22 have 2-col whitespace margins on both sides.
    let row0 = buffer_row(&harness, 0);
    assert!(row0.starts_with("Plan: Release Readiness"));
    assert!(row0.contains('─'));
    assert!(buffer_row(&harness, 1).trim().is_empty());

    for y in 2..22 {
        let row = buffer_row(&harness, y);
        let chars: Vec<char> = row.chars().collect();
        assert_eq!(chars[0], ' ', "left margin col 0 must be blank on row {y}");
        assert_eq!(chars[1], ' ', "left margin col 1 must be blank on row {y}");
        assert_eq!(
            chars[77], ' ',
            "right margin col 77 must be blank on row {y}"
        );
        assert_eq!(
            chars[78], ' ',
            "right margin col 78 must be blank on row {y}"
        );
    }

    // Sections stay separated while tight sibling list items remain adjacent.
    let lines: Vec<&str> = screen.lines().collect();
    let scope_idx = lines.iter().position(|l| l.contains("Scope")).unwrap();
    assert!(
        buffer_row(&harness, scope_idx as u16 + 1)[..79]
            .trim()
            .is_empty(),
        "heading must be separated from list by one blank content row"
    );
    let tight_a = lines
        .iter()
        .position(|l| l.contains("Verify nested wrapped continuation"))
        .unwrap();
    let tight_b = lines
        .iter()
        .position(|l| l.contains("Tight sibling item"))
        .unwrap();
    assert!(
        tight_b > tight_a
            && !(tight_a + 1..tight_b)
                .any(|i| buffer_row(&harness, i as u16)[..79].trim().is_empty()),
        "tight sibling list items must not gain blank separator rows"
    );

    // Reach final row + Links section via End key.
    press_key(&mut harness, KeyCode::End, KeyModifiers::NONE).await;
    let bottom_screen = harness.screen_contents();
    assert!(bottom_screen.contains("FINAL PLAN LINE"));
    assert!(bottom_screen.contains("Links"));
    assert!(bottom_screen.contains("External Spec (https://example.com/spec)"));
}

#[tokio::test]
async fn cid_document_overflowing_buffer_margins_and_outer_scrollbar() {
    let mut harness = TuiTestHarness::with_size(40, 12).await;
    let overflow_doc = (1..=25)
        .map(|i| format!("Line {i:02} with words reaching edge"))
        .collect::<Vec<_>>()
        .join("\n\n");
    show_doc(&mut harness, PLAN_URL, "Overflow Plan", &overflow_doc);

    let buf = harness.terminal.backend().buffer();
    let w = buf.area.width;
    let h = buf.area.height;
    assert_eq!(w, 40);
    assert_eq!(h, 12);

    // Row 1 (immediately below top border) is completely blank, including outer col.
    assert!(buffer_row(&harness, 1).chars().all(|c| c == ' '));

    // Content rows span y = 2..10; cols 0..2 and w-3..w-1 are blank, col w-1 holds scrollbar.
    for y in 2..(h - 2) {
        assert_eq!(buf[(0, y)].symbol(), " ", "left margin col 0 row {y}");
        assert_eq!(buf[(1, y)].symbol(), " ", "left margin col 1 row {y}");
        assert_eq!(
            buf[(w - 3, y)].symbol(),
            " ",
            "right blank margin col {} row {y}",
            w - 3
        );
        assert_eq!(
            buf[(w - 2, y)].symbol(),
            " ",
            "right blank margin col {} row {y}",
            w - 2
        );
        assert_ne!(
            buf[(w - 1, y)].symbol(),
            " ",
            "outermost inner scrollbar col {} must paint chrome on row {y}",
            w - 1
        );
    }
    assert_eq!(buf[(w - 1, 2)].symbol(), "↑");
    assert_eq!(buf[(w - 1, h - 3)].symbol(), "↓");

    // Non-overflowing CID document leaves the outermost column blank and adds no side borders.
    show_doc(
        &mut harness,
        PLAN_URL,
        "Short Plan",
        "# Short\n\nFits easily.",
    );
    let short_buf = harness.terminal.backend().buffer();
    for y in 0..(h - 1) {
        for x in 0..w {
            assert!(
                !matches!(short_buf[(x, y)].symbol(), "│" | "┌" | "┐" | "└" | "┘"),
                "CID document viewer must not draw side/corner border glyphs at ({x}, {y})"
            );
        }
    }
    for y in 2..(h - 2) {
        assert_eq!(short_buf[(w - 1, y)].symbol(), " ");
    }
}

#[tokio::test]
async fn cid_document_wide_narrow_short_and_zero_viewports() {
    let long_line = format!("# Wide Plan\n\n{}\n\nFINAL ROW", "word ".repeat(35).trim());

    // Wide viewport (140x24): no 80-col width cap, 2-col margins + outer scrollbar col when overflowing.
    let mut wide = TuiTestHarness::with_size(140, 24).await;
    show_doc(&mut wide, PLAN_URL, "Wide Plan", &long_line);
    let wide_row = buffer_row(&wide, 4);
    let wide_chars: Vec<char> = wide_row.chars().collect();
    assert!(wide_row.starts_with("  word "));
    assert_eq!(wide_chars[137], ' ');
    assert_eq!(wide_chars[138], ' ');
    assert_eq!(wide_chars[139], ' ');
    assert!(
        wide_row.trim_end().len() > 90,
        "wide content should not cap at 80 cols"
    );

    // Tiny shrinking widths 2..4 keep content >= 1 and rightmost scrollbar when overflowing.
    for w in 2..=4u16 {
        let mut tiny_w = TuiTestHarness::with_size(w, 10).await;
        show_doc(&mut tiny_w, PLAN_URL, "N", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj");
        let buf_w = tiny_w.terminal.backend().buffer();
        assert_eq!(buf_w[(w - 1, 2)].symbol(), "↑");
        assert_eq!(buf_w[(w - 1, 7)].symbol(), "↓");
    }

    // Narrow 5x10: usable=4 -> left=1, content=2, right=1, scrollbar=col 4.
    let mut narrow5 = TuiTestHarness::with_size(5, 10).await;
    show_doc(
        &mut narrow5,
        PLAN_URL,
        "N",
        "abcdef\n\nghijkl\n\nmnopqr\n\nstuvwx\n\nZ",
    );
    let buf5 = narrow5.terminal.backend().buffer();
    assert_eq!(buf5[(0, 2)].symbol(), " ");
    assert_eq!(buf5[(3, 2)].symbol(), " ");
    assert_eq!(buf5[(4, 2)].symbol(), "↑");

    // Ultra-narrow 1x10: content + separate scrollbar cannot fit -> hide chrome, keep scrolling.
    let mut narrow1 = TuiTestHarness::with_size(1, 10).await;
    show_doc(
        &mut narrow1,
        PLAN_URL,
        "N",
        "A\nB\nC\nD\nE\nF\nG\nH\nI\nJ\nK\nZ",
    );
    let screen1 = narrow1.screen_contents();
    assert!(!screen1.contains('↑') && !screen1.contains('↓'));
    assert!(narrow1.tui().app.detail_view_scroll.last_max_position > 0);
    press_key(&mut narrow1, KeyCode::End, KeyModifiers::NONE).await;
    assert!(narrow1.screen_contents().contains('Z'));

    // Very short 40x4 (inner height == 1): top blank margin relaxes so content row renders.
    let mut short4 = TuiTestHarness::with_size(40, 4).await;
    show_doc(&mut short4, PLAN_URL, "Short", "Visible row\n\nSecond row");
    assert!(buffer_row(&short4, 1).contains("Visible row"));

    // Height 3 (inner height == 0) and zero-sized viewports do not panic.
    for (w, h) in [(40, 3), (0, 0), (0, 10), (10, 0)] {
        let mut tiny = TuiTestHarness::with_size(w, h).await;
        show_doc(&mut tiny, PLAN_URL, "Tiny", REPRESENTATIVE_PLAN_FIXTURE);
    }
}

#[tokio::test]
async fn cid_document_resize_cache_consistency_and_final_row_reachable() {
    let mut harness = TuiTestHarness::with_size(100, 14).await;
    let doc = format!(
        "# Resize Test\n\n{}\n\n{}\n\nEND_SENTINEL",
        "alpha beta gamma delta epsilon zeta ".repeat(6).trim(),
        "theta iota kappa lambda mu nu xi ".repeat(6).trim(),
    );
    show_doc(&mut harness, PLAN_URL, "Resize Test", &doc);

    // Post-margin content width at terminal width 100 is 100 - 5 = 95 (not 100).
    assert!(harness
        .tui()
        .app
        .detail_view_scroll
        .cached_heights_for_width(95)
        .is_some());
    assert!(harness
        .tui()
        .app
        .detail_view_scroll
        .cached_heights_for_width(100)
        .is_none());
    let max_wide = harness.tui().app.detail_view_scroll.last_max_position;

    // Resize narrower (40x14 -> content width 35): wrapped height increases and cache uses 35.
    harness.terminal.backend_mut().resize(40, 14);
    harness.render();
    assert!(harness
        .tui()
        .app
        .detail_view_scroll
        .cached_heights_for_width(35)
        .is_some());
    let max_narrow = harness.tui().app.detail_view_scroll.last_max_position;
    assert!(max_narrow > max_wide);

    // Scroll to bottom and confirm final sentinel row lands cleanly on row 11 (above bottom border).
    press_key(&mut harness, KeyCode::End, KeyModifiers::NONE).await;
    assert_eq!(harness.tui().app.detail_view_scroll.position, max_narrow);
    assert!(buffer_row(&harness, 11).contains("END_SENTINEL"));

    // Resize wider again while at bottom: position clamps cleanly and final row stays visible.
    harness.terminal.backend_mut().resize(100, 14);
    harness.render();
    assert_eq!(harness.tui().app.detail_view_scroll.position, max_wide);
    assert!(buffer_row(&harness, 11).contains("END_SENTINEL"));
}

#[tokio::test]
async fn cid_document_keyboard_links_and_non_cid_detail_unchanged() {
    let mut harness = TuiTestHarness::with_size(80, 20).await;
    harness.tui().cid_resolve_override = Some(Arc::new(|url| {
        let md = match url.as_str() {
            PLAN_URL => REPRESENTATIVE_PLAN_FIXTURE,
            TASK_T1_URL => "# T1\n\nTask body.",
            NOTE_IMPL_URL => "# Impl\n\nNote body.",
            other => panic!("unexpected CID link: {other}"),
        };
        Box::pin(async move { Ok(resolved_blob("text/markdown", md.as_bytes().to_vec())) })
    }));

    show_doc(
        &mut harness,
        PLAN_URL,
        "Plan: Release Readiness",
        REPRESENTATIVE_PLAN_FIXTURE,
    );
    assert_eq!(
        harness.tui().app.doc_view.as_ref().unwrap().focused_link,
        Some(0)
    );

    press_key(&mut harness, KeyCode::Tab, KeyModifiers::NONE).await;
    assert_eq!(
        harness.tui().app.doc_view.as_ref().unwrap().focused_link,
        Some(1)
    );

    press_key(&mut harness, KeyCode::BackTab, KeyModifiers::SHIFT).await;
    assert_eq!(
        harness.tui().app.doc_view.as_ref().unwrap().focused_link,
        Some(0)
    );

    press_key(&mut harness, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(
        harness.tui().app.doc_view.as_ref().unwrap().url,
        TASK_T1_URL
    );
    assert!(harness.screen_contents().contains("Task body."));

    press_key(&mut harness, KeyCode::Backspace, KeyModifiers::NONE).await;
    assert_eq!(harness.tui().app.doc_view.as_ref().unwrap().url, PLAN_URL);

    // Non-CID standard detail view keeps prior geometry: no top blank row or 2-col left margin.
    harness.tui().app.doc_view = None;
    harness.tui().app.transcript.clear();
    harness.tui().app.transcript.push(TranscriptItem::UserText {
        text: "Standard detail line".into(),
        seq: Some(1),
        timestamp: None,
    });
    harness.tui().app.transcript_focus = Some(0);
    harness.tui().open_detail_view_for_focused_item();
    harness.render();

    assert!(
        buffer_row(&harness, 1).starts_with("── user ──"),
        "non-CID detail view must keep prior marginless geometry (row 1: {:?})",
        buffer_row(&harness, 1)
    );
    assert!(
        harness.screen_contents().contains("Standard detail line"),
        "non-CID detail view content must still render"
    );
}
