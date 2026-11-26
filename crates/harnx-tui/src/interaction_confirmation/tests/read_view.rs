//! Deterministic regressions for pending confirmations coexisting with read
//! views (#2927 T2): browsing while a tray is pending, the reading→tray
//! return-safety guard, readonly browsing guards, and refresh normalization.
//!
//! Time control uses state timestamps (`last_key_at`, `decision_guard_until`),
//! never wall-clock sleeps.

use super::a2ui::{a2ui_round_entries, sample_a2ui_payload};
use super::{modal, snapshot, tui_with_pending};
use crate::interaction_confirmation::pending_confirmation;
use crate::types::{
    InteractionFocus, ModalState, MonitoredSessionKey, MonitoredSessionState, SubAgentStatus,
    SubAgentView, TranscriptItem, Tui, TuiEvent,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};
use harnx_core::pending_interaction::{PendingInteractionRequest, PendingInteractionSummary};
use std::time::{Duration, Instant};

mod review_cycle;

fn user_item(text: &str) -> TranscriptItem {
    TranscriptItem::UserText {
        text: text.into(),
        seq: None,
        timestamp: None,
    }
}

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn ctrl_d() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

/// A reported Repeat event — legacy terminals also auto-repeat as Press,
/// which is what the return-safety guard covers.
fn repeat(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Repeat,
        state: KeyEventState::NONE,
    }
}

/// Advance the tray's clock past the two-second quiet periods by backdating
/// the idle gate and the return guard.
fn backdate_quiet(tui: &mut Tui, seconds: u64) {
    let past = Instant::now() - Duration::from_secs(seconds);
    let state = modal(tui);
    state.last_key_at = past;
    state.decision_guard_until = Some(past);
}

/// Draw through the real `draw` routing and return the rendered screen.
fn drawn_screen(tui: &mut Tui, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| tui.draw(frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

/// Seed a root prompt row plus a plan link whose nested task link resolves
/// through the cid override, so tests can open and traverse the documents.
fn seed_nested_plan_fixture(tui: &mut Tui) {
    tui.interactions.target = Some(("origin-session".into(), "origin-cluster".into()));
    tui.app.transcript.push(user_item("prompt"));
    tui.app.transcript.push(TranscriptItem::MarkdownLink {
        text: "proposed plan".into(),
        url: "cid:plan:acme/sess01/proposal".into(),
    });
    tui.cid_resolve_override = Some(std::sync::Arc::new(|url: String| {
        Box::pin(async move {
            let text = match url.as_str() {
                "cid:plan:acme/sess01/proposal" => {
                    "# Proposed plan\nSee [the task](cid:plan:acme/sess01/tasks/t1).".to_string()
                }
                "cid:plan:acme/sess01/tasks/t1" => "# Task one\nDo the work.".to_string(),
                other => anyhow::bail!("unexpected url {other}"),
            };
            Ok(harnx_blob_store::ResolvedBlob {
                mime_type: "text/markdown".into(),
                bytes: text.into_bytes(),
                etag: None,
                immutable: true,
            })
        })
    }));
}

async fn fresh_tui() -> Tui {
    let config = crate::tests::test_config();
    Tui::init(&config).await.unwrap()
}

fn pending_snapshot(tui: &mut Tui) {
    let mut entries = super::requests();
    super::ready(&mut entries);
    let pending = pending_confirmation(&entries).unwrap().unwrap();
    snapshot(tui, Some(pending));
}

#[tokio::test]
async fn shared_browse_entry_clears_completions_only_when_target_exists() {
    let mut tui = tui_with_pending().await;
    tui.app.completions = vec![("/help".into(), None)];
    tui.set_input_text("composer draft");
    assert!(!tui.enter_transcript_browsing());
    assert_eq!(
        tui.app.completions.len(),
        1,
        "no-target leaves state intact"
    );
    tui.app.transcript.push(user_item("transcript item"));
    assert!(tui.enter_transcript_browsing());
    assert!(tui.app.completions.is_empty());
    assert_eq!(tui.app.input.lines().join("\n"), "composer draft");
}

#[tokio::test]
async fn up_from_empty_note_browses_and_nonempty_note_stays_editing() {
    let mut tui = tui_with_pending().await;
    assert!(
        modal(&mut tui).decision_guard_until.is_none(),
        "initial presentation keeps existing semantics"
    );
    tui.clear_transcript();
    tui.app.transcript.push(user_item("first prompt"));
    tui.app.transcript.push(user_item("second prompt"));
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);

    tui.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.app.transcript_browsing, "empty-note Up enters browse");
    assert_eq!(tui.app.transcript_focus, Some(1));
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    // Esc returns to the tray; a nonempty note keeps Up as cursor editing.
    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    modal(&mut tui).note.insert_str("draft note");
    tui.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.transcript_browsing, "nonempty note Up edits note");
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "draft note");
}

#[tokio::test]
async fn esc_from_tray_preserves_note_and_returned_enter_stays_guarded() {
    let mut tui = tui_with_pending().await;
    tui.app.transcript.push(user_item("first prompt"));
    tui.app.transcript.push(user_item("second prompt"));
    tui.app.completions = vec![("/help".into(), None)];
    tui.set_input_text("hidden composer draft");
    modal(&mut tui).note.insert_str("answer note");

    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    assert!(tui.app.completions.is_empty());
    let initial_focus = tui.app.transcript_focus.unwrap();
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "answer note");
    tui.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.app.transcript_focus.unwrap() < initial_focus);
    tui.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.app.transcript_focus, Some(initial_focus));
    assert_eq!(
        tui.app.input.lines().join("\n"),
        "hidden composer draft",
        "browse entry never touches the hidden composer"
    );

    // Paste while reading must not edit the hidden note or composer.
    tui.handle_paste("hidden paste".into()).await;
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "answer note");
    assert_eq!(tui.app.input.lines().join("\n"), "hidden composer draft");
    assert!(tui.app.attachments.is_empty());

    // Esc returns to the tray with the return-safety guard armed.
    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_some());
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "answer note");

    // Immediate Enter cannot approve (guard plus idle gate).
    tui.handle_key(enter()).await.unwrap();
    assert!(!modal(&mut tui).submitting);
    assert!(tui.interactions.decisions.is_empty());

    // After two quiet seconds a fresh focused decision works.
    backdate_quiet(&mut tui, 3);
    tui.handle_key(enter()).await.unwrap();
    assert!(modal(&mut tui).submitting, "fresh decision after quiet");
}

#[tokio::test]
async fn reading_ctrl_d_is_refused_then_rebuild_restores_guarded_tray() {
    let mut tui = tui_with_pending().await;
    tui.app.transcript.push(user_item("only row"));
    tui.set_input_text("draft stays");
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);

    // Ctrl+D while reading can neither deny nor exit the TUI.
    tui.handle_key(ctrl_d()).await.unwrap();
    assert!(!tui.app.should_quit);
    assert!(tui.cancellation.is_none());
    assert!(!modal(&mut tui).submitting);
    assert!(tui
        .app
        .footer_notice_text()
        .is_some_and(|notice| notice.contains("Ctrl+D")));

    // A transcript rebuild loses the root browse row: automatic tray
    // restoration, guarded.
    tui.normalize_reading_after_transcript_rebuild();
    assert!(!tui.app.transcript_browsing);
    assert_eq!(tui.app.transcript_focus, None);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_some());
    assert_eq!(tui.app.input.lines().join("\n"), "draft stays");

    // Consecutive Ctrl+D Press events under two seconds apart never deny.
    for _ in 0..3 {
        tui.handle_key(ctrl_d()).await.unwrap();
        assert!(!modal(&mut tui).submitting);
        assert!(tui.interactions.decisions.is_empty());
    }

    // A reported Repeat never submits, even past the guard.
    backdate_quiet(&mut tui, 3);
    tui.handle_key(repeat(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    tui.handle_key(repeat(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(!modal(&mut tui).submitting);

    // After two quiet seconds a fresh focused denial works.
    backdate_quiet(&mut tui, 3);
    tui.handle_key(ctrl_d()).await.unwrap();
    assert!(modal(&mut tui).submitting, "fresh denial after quiet");
}

#[tokio::test]
async fn pending_arrival_preserves_open_document_and_enter_never_approves() {
    let mut tui = fresh_tui().await;
    seed_nested_plan_fixture(&mut tui);

    // Browse and open the plan document before the confirmation arrives.
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    tui.handle_key(enter()).await.unwrap();
    assert!(tui.app.detail_view_open);
    assert!(tui.app.doc_view.is_some());

    // Passive arrival keeps the open document focused.
    pending_snapshot(&mut tui);
    assert!(tui.interaction_reading());
    assert!(tui.app.doc_view.is_some(), "document survives arrival");
    assert!(modal(&mut tui).decision_guard_until.is_none());

    // The document renders unobstructed, with only a compact pending hint.
    let screen = drawn_screen(&mut tui, 80, 20);
    assert!(screen.contains("Proposed plan"), "{screen}");
    assert!(
        screen.contains("Middleware confirmation pending"),
        "{screen}"
    );
    assert!(
        !screen.contains("Allow second?"),
        "tray body stays hidden: {screen}"
    );

    // Enter follows the nested link without approving anything.
    tui.handle_key(enter()).await.unwrap();
    assert!(tui
        .app
        .doc_view
        .as_ref()
        .is_some_and(|view| view.title == "Task one"));
    assert_eq!(tui.app.doc_history.len(), 1);
    assert!(!modal(&mut tui).submitting);

    // Esc walks back one level at a time: nested document, then the plan.
    tui.handle_key(esc()).await.unwrap();
    assert!(tui
        .app
        .doc_view
        .as_ref()
        .is_some_and(|view| view.title == "Proposed plan"));
    tui.handle_key(esc()).await.unwrap();
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.doc_view.is_none());
    assert!(tui.app.transcript_browsing, "browse level resumes");

    // Top-level browse Esc returns to the tray, guarded.
    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_some());
    tui.handle_key(enter()).await.unwrap();
    assert!(!modal(&mut tui).submitting);
    backdate_quiet(&mut tui, 3);
    tui.handle_key(enter()).await.unwrap();
    assert!(modal(&mut tui).submitting, "fresh approval after quiet");
}

#[tokio::test]
async fn rebuild_preserves_nested_document_history_and_back_navigation() {
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
    pending_snapshot(&mut tui);

    // Losing the root browsing row must not close the nested document.
    tui.normalize_reading_after_transcript_rebuild();
    assert!(
        tui.app.doc_view.is_some(),
        "nested document survives rebuild"
    );
    assert_eq!(tui.app.doc_history.len(), 1);
    assert!(tui.interaction_reading());

    // Back navigation still works, and the final Esc restores the tray.
    tui.handle_key(esc()).await.unwrap();
    assert!(tui
        .app
        .doc_view
        .as_ref()
        .is_some_and(|view| view.title == "Proposed plan"));
    tui.handle_key(esc()).await.unwrap();
    assert!(!tui.app.detail_view_open);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_some());
}

fn child_summary() -> PendingInteractionSummary {
    PendingInteractionSummary {
        agent: "child-agent".into(),
        cluster: "child-cluster".into(),
        session_id: "child-session".into(),
        request: PendingInteractionRequest::Interaction {
            interaction_id: "confirm-id".into(),
            request_seq: 14,
            interaction_kind: "confirm".into(),
        },
        tool_name: "bash_exec".into(),
        message: "Allow this child's command?".into(),
        pending_count: 0,
    }
}

fn child_row() -> TranscriptItem {
    TranscriptItem::SubAgentSession {
        key: MonitoredSessionKey {
            agent: "child-agent".into(),
            session_id: "child-session".into(),
            cluster: "child-cluster".into(),
        },
        status: SubAgentStatus::Running,
        invocation_id: Some("parent-call".into()),
        progress: Some(crate::types::SubAgentInvocationProgress::new(
            SubAgentProgress {
                invocation_id: "parent-call".into(),
                agent: "child-agent".into(),
                session_id: "child-session".into(),
                status: SubAgentProgressStatus::Running,
                pending_interaction: Some(child_summary()),
                elapsed_ms: 1000,
                usage: Default::default(),
                tool_call_count: 1,
                title: None,
                tool_call_id: None,
            },
        )),
    }
}

fn open_child_view(tui: &mut Tui) {
    let TranscriptItem::SubAgentSession { key, progress, .. } = child_row() else {
        unreachable!()
    };
    let view = SubAgentView {
        key: key.clone(),
        status: SubAgentStatus::Running,
        progress,
    };
    tui.app.subagent_view_stack.push(view);
    let mut state = MonitoredSessionState::new(SubAgentStatus::Running);
    state.transcript.push(user_item("child work in progress"));
    tui.app.monitored_sessions.insert(key, state);
}

#[tokio::test]
async fn answer_from_open_child_view_focuses_tray_and_esc_resumes_view() {
    let mut tui = fresh_tui().await;
    tui.app.llm_busy = false;
    tui.app.transcript = vec![child_row()];
    open_child_view(&mut tui);

    // Explicit Answer from the open child view focuses its tray.
    tui.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(modal(&mut tui).child.is_some());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(
        !tui.app.subagent_view_stack.is_empty(),
        "child stack preserved"
    );

    // The tray is bounded at the bottom with the child view still visible.
    let screen = drawn_screen(&mut tui, 80, 24);
    assert!(screen.contains("Child confirmation"), "{screen}");
    assert!(screen.contains("child work in progress"), "{screen}");

    // A note can be typed while the tray is focused.
    for ch in ['a', 'p', 'p', 'r', 'o', 'v', 'e'] {
        tui.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "approve");

    // Esc resumes the existing child view instead of selecting a new item.
    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "approve");

    // Esc from the child view closes the last level and restores the tray.
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.subagent_view_stack.is_empty());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_some());

    // Guarded tray refuses immediate decisions; after quiet, the explicit
    // approval carries the correct child identity and the typed note.
    tui.handle_key(enter()).await.unwrap();
    assert!(!modal(&mut tui).submitting);

    let expected = modal(&mut tui).child.clone().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = captured.clone();
    tui.interactions.children.decision_override =
        Some(std::sync::Arc::new(move |request, approved, note| {
            calls.lock().unwrap().push((request, approved, note));
            Box::pin(async { Err("routing failed".into()) })
        }));
    backdate_quiet(&mut tui, 3);
    tui.handle_key(enter()).await.unwrap();
    assert!(modal(&mut tui).submitting);
    let event = tokio::time::timeout(Duration::from_secs(2), tui.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let TuiEvent::Interaction(event) = event else {
        panic!("missing decision event");
    };
    tui.handle_interaction_event(event);
    assert_eq!(
        *captured.lock().unwrap(),
        vec![(expected, true, "approve".into())]
    );
}

#[tokio::test]
async fn readonly_guards_refuse_mutations_even_when_idle() {
    let mut tui = tui_with_pending().await;
    tui.app.llm_busy = false;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("editable prompt"));
    tui.app.transcript.push(TranscriptItem::HandoffSession {
        agent: "daedalus".into(),
        session_id: "abc123".into(),
    });
    tui.set_input_text("composer draft stays");
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    assert_eq!(tui.app.transcript_focus, Some(1), "handoff row focused");

    for code in [KeyCode::Char('e'), KeyCode::Char('i'), KeyCode::Char('r')] {
        tui.handle_key(key(code, KeyModifiers::NONE)).await.unwrap();
        assert_eq!(
            tui.app.footer_notice_text(),
            Some("Answer the pending confirmation first"),
            "key {code:?} refused"
        );
    }
    // Delete must not open a ConfirmDelete modal on top of the tray.
    tui.handle_key(key(KeyCode::Char('d'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmInteraction(_))
    ));
    // Rewind never replaced the tray either.
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmInteraction(_))
    ));

    // Enter on the handoff row refuses session switching (idle turn).
    tui.handle_key(enter()).await.unwrap();
    assert_eq!(
        tui.app.footer_notice_text(),
        Some("Answer the pending confirmation first")
    );
    assert!(
        tui.config.read().session.is_none(),
        "session never switched"
    );
    assert_eq!(
        tui.app.input.lines().join("\n"),
        "composer draft stays",
        "insert never wrote the composer"
    );
}

#[tokio::test]
async fn up_at_first_item_clamps_and_down_past_last_returns_to_tray() {
    let mut tui = tui_with_pending().await;
    tui.clear_transcript();
    tui.app.transcript.push(user_item("only row"));
    tui.set_input_text("draft stays");
    tui.app.history = vec!["older submission".into()];
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    assert_eq!(tui.app.transcript_focus, Some(0));

    // Up at the first item clamps; no composer-history fallback.
    tui.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.app.transcript_focus, Some(0), "clamped at first item");
    assert!(tui.app.transcript_browsing);
    assert!(!tui.app.history_preview);
    assert_eq!(tui.app.input.lines().join("\n"), "draft stays");

    // Down past the last item returns to the tray, guarded.
    tui.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.transcript_browsing);
    assert_eq!(tui.app.transcript_focus, None);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_some());
}

fn swap_to_a2ui(tui: &mut Tui) {
    let payload = sample_a2ui_payload();
    let entries = a2ui_round_entries("a2ui-read-view", payload);
    let pending = pending_confirmation(&entries).unwrap().unwrap();
    snapshot(tui, Some(pending));
}

#[tokio::test]
async fn a2ui_arrows_stay_form_navigation_and_esc_yields_focus_only() {
    let mut tui = tui_with_pending().await;
    tui.app.transcript.push(user_item("row"));
    swap_to_a2ui(&mut tui);
    assert!(modal(&mut tui).a2ui.is_some());

    // Down stays A2UI choice navigation.
    tui.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!tui.app.transcript_browsing, "arrows stay in the form");
    let form = modal(&mut tui).a2ui.as_ref().unwrap();
    assert_eq!(
        form.focus,
        crate::interaction_confirmation::a2ui::FocusTarget::Field(0)
    );

    // Esc yields focus to browse without changing the selected field.
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
    let form = modal(&mut tui).a2ui.as_ref().unwrap();
    assert_eq!(
        form.focus,
        crate::interaction_confirmation::a2ui::FocusTarget::Field(0)
    );

    // Esc returns to the tray with the form focus and guard preserved.
    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_some());
    let form = modal(&mut tui).a2ui.as_ref().unwrap();
    assert_eq!(
        form.focus,
        crate::interaction_confirmation::a2ui::FocusTarget::Field(0)
    );
}

#[tokio::test]
async fn a2ui_enter_after_return_needs_quiet_before_submitting() {
    let mut tui = tui_with_pending().await;
    tui.app.transcript.push(user_item("row"));
    swap_to_a2ui(&mut tui);

    // Focus the primary button.
    for _ in 0..2 {
        tui.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert_eq!(
        modal(&mut tui).a2ui.as_ref().unwrap().focus,
        crate::interaction_confirmation::a2ui::FocusTarget::Button(0)
    );

    // Read and return: the guard holds.
    tui.handle_key(esc()).await.unwrap();
    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);

    // Repeated Enter Press events under two seconds apart never submit.
    for _ in 0..3 {
        tui.handle_key(enter()).await.unwrap();
        assert!(!modal(&mut tui).submitting);
        assert!(tui.interactions.decisions.is_empty());
    }
    // A reported Repeat never submits either.
    tui.handle_key(repeat(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!modal(&mut tui).submitting);

    // After two quiet seconds a fresh focused submit works.
    backdate_quiet(&mut tui, 3);
    tui.handle_key(enter()).await.unwrap();
    assert!(modal(&mut tui).submitting);
    assert_eq!(tui.interactions.decisions.len(), 1);
}

#[tokio::test]
async fn ctrl_c_while_reading_keeps_interruption_semantics() {
    let mut tui = tui_with_pending().await;
    tui.app.transcript.push(user_item("row"));
    tui.exit_cancel_factory = std::sync::Arc::new(|_, _, _, _| Box::pin(std::future::pending()));
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.transcript_browsing);

    tui.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(
        tui.app.modal.is_none(),
        "tray cleared like a focused Ctrl+C"
    );
    assert!(tui.has_root_cancellation(), "durable interruption started");
}

#[tokio::test]
async fn other_modal_kinds_keep_their_existing_esc_behavior() {
    let mut tui = fresh_tui().await;
    tui.app.transcript.push(user_item("row"));
    tui.app.modal = Some(ModalState::ConfirmDelete { from: 1, to: 1 });
    tui.handle_key(esc()).await.unwrap();
    assert!(tui.app.modal.is_none(), "ConfirmDelete dismissed as before");
    assert!(
        !tui.app.transcript_browsing,
        "no browse entry for other modals"
    );
}

#[tokio::test]
async fn url_modal_up_enters_browse_and_enter_stays_guarded_after_return() {
    let mut tui = tui_with_pending().await;
    let mut pending = modal(&mut tui).pending.clone();
    pending.kind = "url".into();
    pending.payload = serde_json::json!({
        "url": "https://example.com/connect/abc",
        "message": "Connect Jira"
    });
    tui.app.modal = None;
    tui.interactions.pending = None;
    tui.app.transcript.push(user_item("row"));
    snapshot(&mut tui, Some(pending));

    // url modals render no note area; Up still yields to browsing.
    tui.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.app.transcript_browsing);
    tui.handle_key(esc()).await.unwrap();
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    assert!(modal(&mut tui).decision_guard_until.is_some());

    // Immediate Enter cannot continue the url interaction.
    tui.handle_key(enter()).await.unwrap();
    assert!(!modal(&mut tui).submitting);
    backdate_quiet(&mut tui, 3);
    tui.handle_key(enter()).await.unwrap();
    assert!(modal(&mut tui).submitting, "fresh continue after quiet");
}
