//! Mandatory review-cycle regressions (B1/B2/B3/P1 and THALIA-1..6, plus
//! cycle-2 C2-B1/C2-B2), split from read_view.rs to keep each test file under
//! the file-length budget. Shared fixtures and time-control helpers live in
//! read_view.rs.

use super::super::{ready, requests, response};
use super::{
    backdate_quiet, child_row, ctrl_d, drawn_screen, enter, esc, fresh_tui, key, modal,
    open_child_view, pending_confirmation, pending_snapshot, repeat, seed_nested_plan_fixture,
    snapshot, swap_to_a2ui, tui_with_pending, user_item,
};
use crate::types::{InteractionFocus, ModalState, TranscriptItem, Tui};
use crossterm::event::{KeyCode, KeyModifiers};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Mandatory review cycle-1 regressions (B1/B2/B3/P1 and THALIA-1..6).
// ---------------------------------------------------------------------------

/// Enter browsing and return to the tray so the return-safety guard is armed.
async fn arm_guard_via_browse(tui: &mut Tui) {
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(tui).decision_guard_until.is_some());
}

/// Pin the guard near expiry (300 ms) and backdate the idle gate, so any
/// restart is observable as a deadline extension.
fn near_expiry_deadline(tui: &mut Tui) -> Instant {
    let deadline = Instant::now() + Duration::from_millis(300);
    modal(tui).last_key_at = Instant::now() - Duration::from_secs(5);
    modal(tui).decision_guard_until = Some(deadline);
    deadline
}

/// The successor pending request ("first") that follows "second" in the
/// shared fixture once "second" is answered.
fn successor_pending() -> crate::interaction_confirmation::PendingConfirmation {
    let mut entries = requests();
    ready(&mut entries);
    entries.push(response(5, "second", 3));
    let pending = pending_confirmation(&entries).unwrap().unwrap();
    assert_eq!(pending.key.id, "first");
    pending
}

async fn production_compaction(tui: &mut Tui) {
    tui.render_agent_event(harnx_core::event::AgentEvent::Session(
        harnx_core::event::SessionEvent::CompactingCompleted {
            compaction_id: None,
            outcome: harnx_core::session::CompactOutcome::Compacted,
        },
    ))
    .await;
    tui.flush_history_work_for_test().await;
}

async fn production_refresh(tui: &mut Tui) {
    // Stamp freshness is not under test; live is accepted unconditionally.
    let stamp = crate::event_isolation::EventStamp::live(&tui.live_events);
    crate::Tui::refresh_shared_session_transcript(tui, &stamp).await;
    tui.flush_history_work_for_test().await;
}

/// B1: an explicit child Answer keeps tray focus, its guard-free deadline,
/// and the retained child stack across a real transcript refresh.
#[tokio::test]
async fn background_refresh_preserves_answer_tray_focus_and_deadline() {
    let mut tui = fresh_tui().await;
    tui.app.llm_busy = false;
    tui.app.transcript = vec![child_row()];
    open_child_view(&mut tui);
    tui.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_none());

    production_refresh(&mut tui).await;

    assert_eq!(
        tui.app.interaction_focus,
        InteractionFocus::Tray,
        "refresh must not steal Answer-tray focus"
    );
    assert!(
        modal(&mut tui).decision_guard_until.is_none(),
        "refresh is not a reading->tray transition; no spurious guard"
    );
    assert!(!tui.app.subagent_view_stack.is_empty());
    assert!(modal(&mut tui).child.is_some());
}

/// B1: a plain focused tray with no read view never acquires the return
/// guard from a background refresh.
#[tokio::test]
async fn background_refresh_keeps_plain_tray_unarmed() {
    let mut tui = tui_with_pending().await;
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_none());

    production_refresh(&mut tui).await;

    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(
        modal(&mut tui).decision_guard_until.is_none(),
        "initial presentation semantics preserved across refresh"
    );
}

/// B1 via the real compaction path: the Answer tray keeps focus and stays
/// unarmed; the retained child view survives the rebuild.
#[tokio::test]
async fn compaction_preserves_answer_tray_focus_without_guard() {
    let mut tui = fresh_tui().await;
    tui.app.llm_busy = false;
    tui.app.transcript = vec![child_row()];
    open_child_view(&mut tui);
    tui.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);

    production_compaction(&mut tui).await;

    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_none());
    assert!(!tui.app.subagent_view_stack.is_empty());
    assert!(modal(&mut tui).child.is_some());
}

/// B2: compaction clears the root browse flag even without a pending modal,
/// so a later passive arrival cannot adopt a stale browse state and route
/// arrows into the hidden composer or history.
#[tokio::test]
async fn compaction_clears_stale_browse_before_passive_arrival() {
    let mut tui = fresh_tui().await;
    tui.app.llm_busy = false;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("only row"));
    tui.set_input_text("hidden draft");
    tui.app.history = vec!["older submission".into()];
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    assert!(tui.app.transcript_focus.is_some());

    production_compaction(&mut tui).await;

    assert!(!tui.app.transcript_browsing, "stale flag cleared");
    assert_eq!(tui.app.transcript_focus, None);

    // Passive arrival finds no read view: the tray takes focus, and arrows
    // can only touch the visible note, never the hidden composer/history.
    pending_snapshot(&mut tui);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    tui.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.history_preview);
    tui.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.history_preview);
    assert_eq!(
        tui.app.input.lines().join("\n"),
        "hidden draft",
        "hidden composer untouched after compaction + passive arrival"
    );
}

/// C2-B1 (cycle2): a matching-generation empty snapshot while a nested
/// document owns Reading resolves the request — modal and pending clear to
/// the default Tray focus — while the document, its history and Esc back
/// navigation stay intact.
#[tokio::test]
async fn empty_snapshot_resolves_reading_and_keeps_document_navigable() {
    let mut tui = fresh_tui().await;
    seed_nested_plan_fixture(&mut tui);
    tui.handle_key(esc()).await.unwrap();
    tui.handle_key(enter()).await.unwrap();
    tui.handle_key(enter()).await.unwrap();
    assert!(tui
        .app
        .doc_view
        .as_ref()
        .is_some_and(|view| view.title == "Task one"));
    assert_eq!(tui.app.doc_history.len(), 1);
    pending_snapshot(&mut tui);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    // External resolution: empty snapshot with the current generation.
    snapshot(&mut tui, None);

    assert!(tui.app.modal.is_none(), "pending request resolved");
    assert!(tui.interactions.pending.is_none(), "client pending cleared");
    assert_eq!(
        tui.app.interaction_focus,
        InteractionFocus::Tray,
        "stale reading focus reset by resolution"
    );
    assert!(
        tui.app
            .doc_view
            .as_ref()
            .is_some_and(|view| view.title == "Task one"),
        "open document survives resolution"
    );
    assert_eq!(tui.app.doc_history.len(), 1, "document history retained");

    // Esc still walks back one level at a time.
    tui.handle_key(esc()).await.unwrap();
    assert!(tui
        .app
        .doc_view
        .as_ref()
        .is_some_and(|view| view.title == "Proposed plan"));
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.doc_view.is_none());
    assert!(tui.app.transcript_browsing, "browse level resumes");
}

/// B2 defense in depth: even with the browse invariant violated (flag set,
/// no focused row), pending-reading arrows show the notice and never reach
/// the hidden composer or history preview.
#[tokio::test]
async fn forced_stale_reading_arrows_never_touch_hidden_composer() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("row"));
    tui.set_input_text("hidden draft");
    tui.app.history = vec!["older submission".into()];
    // Force the stale state the compaction fix prevents.
    tui.app.transcript_browsing = true;
    tui.app.transcript_focus = None;
    tui.app.interaction_focus = InteractionFocus::Reading;

    tui.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        tui.app.footer_notice_text(),
        Some("Answer the pending confirmation first")
    );
    assert!(!tui.app.history_preview);
    tui.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.history_preview);
    assert_eq!(
        tui.app.input.lines().join("\n"),
        "hidden draft",
        "arrows never edited the hidden composer"
    );
}

/// C2-B2 (cycle2): Down's stale-reading refusal branch executes on its own.
/// A fresh stale-Reading fixture presses Down first — no prior Up run that
/// could have settled Reading to Tray and left a stale notice — and asserts
/// the exact pending refusal plus untouched hidden draft/history.
#[tokio::test]
async fn stale_reading_down_first_shows_refusal_not_composer() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("row"));
    tui.set_input_text("hidden draft");
    tui.app.history = vec!["older submission".into()];
    // Force the stale state the compaction fix prevents, in a fresh Tui so
    // Down is the first key Reading ever sees.
    tui.app.footer_notice = None;
    tui.app.transcript_browsing = true;
    tui.app.transcript_focus = None;
    tui.app.interaction_focus = InteractionFocus::Reading;

    tui.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(
        tui.app.footer_notice_text(),
        Some("Answer the pending confirmation first"),
        "Down itself set the pending refusal notice"
    );
    assert!(!tui.app.history_preview, "no history preview opened");
    assert_eq!(
        tui.app.input.lines().join("\n"),
        "hidden draft",
        "Down never edited the hidden composer"
    );
    assert_eq!(
        tui.app.history,
        vec!["older submission".to_string()],
        "history untouched"
    );
    // The refusal branch kept the pending request intact; the post-key
    // settle then resolves the stale read state to the guarded tray.
    assert!(tui.app.modal.is_some(), "same pending request retained");
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
}

/// THALIA-1: Enter on a ready UserSurface row while a request is pending is
/// refused; the surface form never opens and the same request stays.
#[tokio::test]
async fn pending_enter_on_usersurface_row_is_refused() {
    let mut tui = tui_with_pending().await;
    tui.app.llm_busy = false;
    tui.clear_transcript();
    tui.app.transcript.push(TranscriptItem::UserSurface {
        surface_id: "surf-1".into(),
        payload: serde_json::json!({"summary": "pick one"}),
        resolved: None,
    });
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    let request_key = modal(&mut tui).pending.key.clone();
    tui.app.footer_notice = None;

    tui.handle_key(enter()).await.unwrap();

    assert_eq!(
        tui.app.footer_notice_text(),
        Some("Answer the pending confirmation first"),
        "pending-specific refusal"
    );
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("surface action form must not open");
    };
    assert_eq!(state.pending.key, request_key, "same request retained");
}

/// THALIA-2: near-expiry refused Press, reported Repeat, ordinary input and
/// guarded-tray paste all restart the quiet clock; a guarded A2UI button
/// Space is refused too, and a fresh decision works after quiet.
#[tokio::test]
async fn guard_restarts_on_near_expiry_input_and_paste() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("row"));
    arm_guard_via_browse(&mut tui).await;

    // Refused Press restarts a near-expiry window.
    let near = near_expiry_deadline(&mut tui);
    tui.handle_key(enter()).await.unwrap();
    assert!(!modal(&mut tui).submitting);
    let extended = modal(&mut tui).decision_guard_until.unwrap();
    assert!(
        extended > near + Duration::from_secs(1),
        "refused Press extended the window"
    );

    // Reported Repeat does the same.
    let near = near_expiry_deadline(&mut tui);
    tui.handle_key(repeat(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(modal(&mut tui).decision_guard_until.unwrap() > near + Duration::from_secs(1));

    // Ordinary input is an intervening key too.
    let near = near_expiry_deadline(&mut tui);
    tui.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(modal(&mut tui).decision_guard_until.unwrap() > near + Duration::from_secs(1));
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "x");

    // Paste into the guarded tray extends the window and reaches the note.
    let near = near_expiry_deadline(&mut tui);
    tui.handle_paste("pasted".into()).await;
    assert!(modal(&mut tui).decision_guard_until.unwrap() > near + Duration::from_secs(1));
    assert!(modal(&mut tui).note.lines().join("\n").contains("pasted"));
}

/// THALIA-2 (A2UI): guarded button Space is refused while extending the
/// window, then submits after the quiet period.
#[tokio::test]
async fn guarded_a2ui_button_space_refused_then_fresh_after_quiet() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("row"));
    swap_to_a2ui(&mut tui);
    arm_guard_via_browse(&mut tui).await;
    let Some(form) = modal(&mut tui).a2ui.as_mut() else {
        panic!("a2ui form missing");
    };
    form.focus = crate::interaction_confirmation::a2ui::FocusTarget::Button(0);

    let near = near_expiry_deadline(&mut tui);
    tui.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!modal(&mut tui).submitting, "guarded Space never submits");
    assert!(modal(&mut tui).decision_guard_until.unwrap() > near + Duration::from_secs(1));

    // After quiet, the same Space submits.
    backdate_quiet(&mut tui, 3);
    tui.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        modal(&mut tui).submitting,
        "fresh Space after quiet submits"
    );
}

/// THALIA-3: A2UI Up with a navigable transcript present stays form
/// navigation and never enters browsing.
#[tokio::test]
async fn a2ui_up_stays_form_navigation_with_navigable_rows() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("navigable row"));
    swap_to_a2ui(&mut tui);

    tui.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.transcript_browsing, "Up never enters browse");
    assert_eq!(tui.app.transcript_focus, None);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    let form = modal(&mut tui).a2ui.as_ref().unwrap();
    assert_eq!(
        form.focus,
        crate::interaction_confirmation::a2ui::FocusTarget::Field(0),
        "form focus preserved"
    );

    // Down mirrors the same isolation.
    tui.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.transcript_browsing);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
}

/// THALIA-4 / ERATO-06: the guarded countdown renders on the focused tray
/// (plain and A2UI renderers) and disappears once the window expires.
#[tokio::test]
async fn guarded_tray_renders_countdown_until_quiet() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("row"));
    arm_guard_via_browse(&mut tui).await;

    let guarded = drawn_screen(&mut tui, 80, 24);
    assert!(guarded.contains("unlock in"), "{guarded}");
    backdate_quiet(&mut tui, 3);
    let quiet = drawn_screen(&mut tui, 80, 24);
    assert!(!quiet.contains("unlock in"), "{quiet}");

    // A2UI renderer carries the same countdown.
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("row"));
    swap_to_a2ui(&mut tui);
    arm_guard_via_browse(&mut tui).await;
    let guarded = drawn_screen(&mut tui, 80, 24);
    assert!(guarded.contains("unlock in"), "a2ui guard hint: {guarded}");
    backdate_quiet(&mut tui, 3);
    let quiet = drawn_screen(&mut tui, 80, 24);
    assert!(!quiet.contains("unlock in"), "a2ui quiet footer: {quiet}");
}

/// THALIA-5: a passive successor snapshot while a nested document is open
/// replaces the request identity but keeps the document and reading focus;
/// real refresh and compaction paths then preserve the same view.
#[tokio::test]
async fn snapshot_replacement_and_rebuilds_retain_open_document() {
    let mut tui = fresh_tui().await;
    seed_nested_plan_fixture(&mut tui);
    tui.handle_key(esc()).await.unwrap();
    tui.handle_key(enter()).await.unwrap();
    assert!(tui.app.doc_view.is_some());
    pending_snapshot(&mut tui);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    // Production snapshot replacement while reading.
    snapshot(&mut tui, Some(successor_pending()));
    assert_eq!(modal(&mut tui).pending.key.id, "first");
    assert!(tui.app.doc_view.is_some(), "document survives replacement");
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
    assert!(modal(&mut tui).decision_guard_until.is_none());

    // Real refresh and compaction keep the independently loaded document.
    production_refresh(&mut tui).await;
    assert!(tui.app.doc_view.is_some());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
    production_compaction(&mut tui).await;
    assert!(tui.app.doc_view.is_some());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
}

/// P1: a passive successor presentation inherits the outstanding return
/// guard — same deadline, not re-armed — even across an intervening empty
/// snapshot; a genuine initial presentation stays unarmed.
#[tokio::test]
async fn successor_snapshot_carries_unexpired_return_guard() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("row"));
    arm_guard_via_browse(&mut tui).await;
    let armed = modal(&mut tui).decision_guard_until.unwrap();

    snapshot(&mut tui, Some(successor_pending()));
    assert_eq!(modal(&mut tui).pending.key.id, "first");
    assert_eq!(
        modal(&mut tui).decision_guard_until,
        Some(armed),
        "carried window preserved, not re-armed"
    );
    // The carried guard still refuses a decision.
    tui.handle_key(enter()).await.unwrap();
    assert!(!modal(&mut tui).submitting);

    // Intervening empty snapshot: the window survives until the successor.
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("row"));
    arm_guard_via_browse(&mut tui).await;
    let armed = modal(&mut tui).decision_guard_until.unwrap();
    snapshot(&mut tui, None);
    assert!(tui.app.modal.is_none());
    snapshot(&mut tui, Some(successor_pending()));
    assert_eq!(modal(&mut tui).pending.key.id, "first");
    assert_eq!(modal(&mut tui).decision_guard_until, Some(armed));

    // True initial presentation keeps existing semantics (no guard).
    let mut fresh = tui_with_pending().await;
    assert!(modal(&mut fresh).decision_guard_until.is_none());
}

/// THALIA-6: delete on an idle sequenced editable row is refused with the
/// pending-specific notice and never opens ConfirmDelete.
#[tokio::test]
async fn delete_on_sequenced_row_is_pending_refused() {
    let mut tui = tui_with_pending().await;
    tui.app.llm_busy = false;
    tui.clear_transcript();
    tui.app.transcript.push(TranscriptItem::UserText {
        text: "editable prompt".into(),
        seq: Some(5),
        timestamp: None,
    });
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    let request_key = modal(&mut tui).pending.key.clone();
    tui.app.footer_notice = None;

    tui.handle_key(key(KeyCode::Char('d'), KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(
        tui.app.footer_notice_text(),
        Some("Answer the pending confirmation first"),
        "pending-specific refusal, not the ordinary can't-delete notice"
    );
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("ConfirmDelete must not open");
    };
    assert_eq!(state.pending.key, request_key);
}

/// B3: a refusal inside an open plan document is visible on the rendered
/// screen, not only in App state.
#[tokio::test]
async fn document_refusal_notice_is_visible_on_screen() {
    let mut tui = fresh_tui().await;
    seed_nested_plan_fixture(&mut tui);
    tui.handle_key(esc()).await.unwrap();
    tui.handle_key(enter()).await.unwrap();
    assert!(tui.app.doc_view.is_some());
    pending_snapshot(&mut tui);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    tui.handle_key(ctrl_d()).await.unwrap();
    assert_eq!(
        tui.app.footer_notice_text(),
        Some("Ctrl+D answers in the tray; Esc returns there")
    );
    let screen = drawn_screen(&mut tui, 80, 20);
    assert!(
        screen.contains("Ctrl+D answers in the tray"),
        "notice invisible: {screen}"
    );
}

/// B3: Esc on a tray with nothing navigable shows its no-target notice on
/// the rendered screen.
#[tokio::test]
async fn no_target_tray_notice_is_visible_on_screen() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();

    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.footer_notice_text(), Some("Nothing to browse"));
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    let screen = drawn_screen(&mut tui, 80, 24);
    assert!(screen.contains("Nothing to browse"), "invisible: {screen}");
}

/// B3: a reading refusal while a child view is open is visible on the
/// rendered child screen, not only in App state.
#[tokio::test]
async fn child_view_refusal_notice_is_visible_on_screen() {
    let mut tui = fresh_tui().await;
    tui.app.llm_busy = false;
    tui.app.transcript = vec![child_row()];
    open_child_view(&mut tui);
    pending_snapshot(&mut tui);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    tui.handle_key(ctrl_d()).await.unwrap();
    assert_eq!(
        tui.app.footer_notice_text(),
        Some("Ctrl+D answers in the tray; Esc returns there")
    );
    let screen = drawn_screen(&mut tui, 80, 24);
    assert!(
        screen.contains("Ctrl+D answers in the tray"),
        "notice invisible in child view: {screen}"
    );
}

/// B3: a reading refusal while the compaction archive is open is visible on
/// the rendered archive screen.
#[tokio::test]
async fn archive_refusal_notice_is_visible_on_screen() {
    let mut tui = fresh_tui().await;
    tui.app.llm_busy = false;
    tui.app.archive_browser = Some(crate::compaction_history::ArchiveView::browser_for_tests(
        vec![user_item("archived row")],
    ));
    pending_snapshot(&mut tui);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    tui.handle_key(ctrl_d()).await.unwrap();
    assert_eq!(
        tui.app.footer_notice_text(),
        Some("Ctrl+D answers in the tray; Esc returns there")
    );
    let screen = drawn_screen(&mut tui, 80, 24);
    assert!(
        screen.contains("Ctrl+D answers in the tray"),
        "notice invisible in archive view: {screen}"
    );
}

/// Rebase integration: background refresh and compaction rebuilds keep an open
/// compaction archive browser alive while a confirmation is pending.
#[tokio::test]
async fn background_rebuild_preserves_archive_browser_while_interaction_pending() {
    let mut tui = fresh_tui().await;
    tui.app.llm_busy = false;
    tui.app.archive_browser = Some(crate::compaction_history::ArchiveView::browser_for_tests(
        vec![user_item("archived row")],
    ));
    pending_snapshot(&mut tui);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    production_refresh(&mut tui).await;
    assert!(tui.app.archive_browser.is_some());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    production_compaction(&mut tui).await;
    assert!(tui.app.archive_browser.is_some());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
}
