use super::decision::decision_response;
use super::*;
use harnx_core::session::SessionLogEntry;
use serde_json::json;
mod a2ui;
mod read_view;
mod ui;

fn request(seq: u64, id: &str, kind: &str) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::InteractionRequested {
            tool_round_seq: 1,
            tool_call_id: format!("call-{id}"),
            interaction_id: id.into(),
            producer: "middleware:test".into(),
            step_path: vec![],
            key: id.into(),
            kind: kind.into(),
            payload: json!({"message": format!("Allow {id}?"), "private": "must not be shown"}),
            summary: "fallback summary".into(),
            chain_version: "v1".into(),
            checkpoint: "private-checkpoint".into(),
            fence_token: 1,
        },
    )
}

fn requests() -> Vec<(u64, SessionLogEntry)> {
    vec![
        (
            1,
            SessionLogEntry::ToolCalls {
                calls: vec![],
                text: String::new(),
                thought: None,
                timestamp: None,
                fence_token: Some(1),
            },
        ),
        request(2, "first", "confirm"),
        request(3, "second", "confirm"),
    ]
}

fn ready(entries: &mut Vec<(u64, SessionLogEntry)>) {
    entries.push((
        4,
        SessionLogEntry::SuspensionReady {
            tool_round_seq: 1,
            pending: vec!["second".into(), "first".into()],
            fence_token: 1,
        },
    ));
}

fn response(seq: u64, id: &str, request_seq: u64) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::InteractionResponse {
            request_seq,
            interaction_id: id.into(),
            tool_call_id: format!("call-{id}"),
            response: decision_response(true, String::new()),
            fence_token: 1,
        },
    )
}

#[test]
fn confirmation_waits_for_ready_and_follows_manifest_not_request_order() {
    let mut entries = requests();
    assert!(pending_confirmation(&entries).unwrap().is_none());
    ready(&mut entries);
    let pending = pending_confirmation(&entries).unwrap().unwrap();
    assert_eq!(pending.key.id, "second");
    assert_eq!(pending.key.request_seq, 3);
    assert_eq!(pending.message, "Allow second?");
    assert_eq!(pending.pending_count, 1);
    assert!(!format!("{pending:?}").contains("private"));
    entries.push(response(5, "second", 3));
    let next = pending_confirmation(&entries).unwrap().unwrap();
    assert_eq!(next.key.id, "first");
    assert_eq!(next.pending_count, 0);
    entries.push(response(6, "first", 2));
    assert!(pending_confirmation(&entries).unwrap().is_none());
}

async fn tui_with_pending() -> Tui {
    let config = crate::tests::test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.modal = None;
    tui.interactions.target = Some(("origin-session".into(), "origin-cluster".into()));
    let mut entries = requests();
    ready(&mut entries);
    snapshot(&mut tui, pending_confirmation(&entries).unwrap());
    tui
}

fn snapshot(tui: &mut Tui, pending: Option<PendingConfirmation>) {
    tui.handle_interaction_event(InteractionEvent::Snapshot {
        generation: tui.interactions.generation,
        pending: Ok(pending),
    });
}

fn modal(tui: &mut Tui) -> &mut InteractionModal {
    let Some(ModalState::ConfirmInteraction(state)) = tui.app.modal.as_mut() else {
        panic!("interaction modal missing");
    };
    state
}

/// Draw the modal once on a test terminal of the given size and return the
/// rendered screen as one string.
fn drawn_screen(tui: &mut Tui, state: &mut InteractionModal, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| tui.render_interaction_modal(frame, frame.area(), state))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

/// Tiny terminal areas must render without panicking.
fn drawn_screen_at_tiny_sizes(tui: &mut Tui, state: &mut InteractionModal) {
    for (width, height) in [(40, 6), (8, 3), (1, 1), (0, 0)] {
        let _ = drawn_screen(tui, state, width, height);
    }
}

fn decide(tui: &mut Tui, key: InteractionKey, result: InteractionDecisionOutcome) {
    tui.handle_interaction_event(InteractionEvent::Decided {
        generation: tui.interactions.generation,
        target: tui.interactions.target.clone().unwrap(),
        key,
        result: Ok(result),
    });
}

#[tokio::test]
async fn accepted_response_waits_for_new_head_and_late_reply_does_not_dismiss_it() {
    let mut tui = tui_with_pending().await;
    let head = modal(&mut tui).pending.clone();
    decide(
        &mut tui,
        head.key.clone(),
        InteractionDecisionOutcome::Applied,
    );
    assert!(tui.app.modal.is_none());
    snapshot(&mut tui, Some(head.clone()));
    assert!(
        tui.app.modal.is_none(),
        "old observer snapshot cannot recreate accepted modal"
    );
    let mut entries = requests();
    ready(&mut entries);
    entries.push(response(5, "second", 3));
    snapshot(&mut tui, pending_confirmation(&entries).unwrap());
    modal(&mut tui).note.insert_str("next draft");
    decide(
        &mut tui,
        head.key,
        InteractionDecisionOutcome::AlreadyApplied,
    );
    assert_eq!(modal(&mut tui).pending.key.id, "first");
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "next draft");
}

#[tokio::test]
async fn rejected_outcomes_are_visible_and_keep_note_for_retry() {
    let mut tui = tui_with_pending().await;
    modal(&mut tui).note.insert_str("reason to deny");
    for outcome in [
        InteractionDecisionOutcome::Conflicting,
        InteractionDecisionOutcome::WrongSequence,
        InteractionDecisionOutcome::Stale,
        InteractionDecisionOutcome::AfterComplete,
        InteractionDecisionOutcome::LeaseLost,
    ] {
        let key = modal(&mut tui).pending.key.clone();
        decide(&mut tui, key, outcome);
        assert!(modal(&mut tui).error.is_some());
        assert_eq!(modal(&mut tui).note.lines().join("\n"), "reason to deny");
        assert!(
            matches!(tui.app.transcript.last(), Some(TranscriptItem::ErrorText(text)) if text.contains("response rejected"))
        );
    }
}

#[tokio::test]
async fn late_conflict_is_visible_without_touching_new_request() {
    let mut tui = tui_with_pending().await;
    let key = modal(&mut tui).pending.key.clone();
    let mut next = modal(&mut tui).pending.clone();
    next.key = InteractionKey {
        id: "new-request".into(),
        request_seq: 8,
    };
    snapshot(&mut tui, Some(next));
    decide(&mut tui, key, InteractionDecisionOutcome::Conflicting);
    assert_eq!(modal(&mut tui).pending.key.id, "new-request");
    assert!(modal(&mut tui).error.is_none());
    assert!(
        matches!(tui.app.transcript.last(), Some(TranscriptItem::ErrorText(text)) if text.contains("conflicting"))
    );
}

#[tokio::test]
async fn unsupported_kind_is_deny_only_even_for_direct_submit() {
    let mut tui = tui_with_pending().await;
    modal(&mut tui).pending.kind = "future-form".into();
    tui.submit_interaction(true);
    assert!(!modal(&mut tui).submitting);
    assert!(tui.interactions.decisions.is_empty());
}

#[tokio::test]
async fn url_modal_renders_the_link_with_continue_and_cancel() {
    let mut entries = requests();
    ready(&mut entries);
    let mut pending = pending_confirmation(&entries).unwrap().unwrap();
    pending.kind = "url".into();
    pending.message = "Connect Jira".into();
    pending.payload = json!({
        "url": "https://example.com/connect/abc",
        "message": "Connect Jira"
    });
    let config = crate::tests::test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.modal = None;
    tui.interactions.target = Some(("origin-session".into(), "origin-cluster".into()));
    snapshot(&mut tui, Some(pending));
    let mut state = modal(&mut tui).clone();
    let screen = drawn_screen(&mut tui, &mut state, 100, 12);
    assert!(screen.contains("Connect Jira"), "{screen}");
    // The URL is the link text: always visible (plain-text fallback).
    assert!(
        screen.contains("https://example.com/connect/abc"),
        "{screen}"
    );
    assert!(screen.contains("ENTER continue"), "{screen}");
    assert!(screen.contains("Ctrl+D cancel"), "{screen}");
    assert!(!screen.contains("not supported by this client"), "{screen}");
    // No note area: the stored response is the action alone.
    assert!(!screen.contains("Note (optional)"), "{screen}");
    drawn_screen_at_tiny_sizes(&mut tui, &mut state);
}

#[tokio::test]
async fn url_modal_wrapped_at_narrow_widths_renders_in_backend_without_target_corruption() {
    let mut entries = requests();
    ready(&mut entries);
    let mut pending = pending_confirmation(&entries).unwrap().unwrap();
    pending.kind = "url".into();
    pending.message = "Connect Jira".into();
    let long_url = "https://example.com/oauth/authorize?client_id=12345&redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback&state=xyz";
    pending.payload = json!({
        "url": long_url,
        "message": "Connect Jira"
    });
    let config = crate::tests::test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.modal = None;
    tui.interactions.target = Some(("origin-session".into(), "origin-cluster".into()));
    snapshot(&mut tui, Some(pending));
    let mut state = modal(&mut tui).clone();

    for width in [30, 45, 60, 90] {
        let screen = drawn_screen(&mut tui, &mut state, width, 14);
        assert!(screen.contains("Connect Jira"), "width {width}: {screen}");
        assert!(screen.contains("ENTER continue"), "width {width}: {screen}");
    }
}

#[test]
fn url_actions_map_enter_to_continue_and_ctrl_d_to_cancel() {
    use super::decision::resolve_decision_action;
    assert_eq!(resolve_decision_action("url", true), Some("continue"));
    assert_eq!(resolve_decision_action("url", false), Some("cancel"));
    assert_eq!(resolve_decision_action("confirm", true), Some("approve"));
    assert_eq!(resolve_decision_action("confirm", false), Some("deny"));
    assert_eq!(resolve_decision_action("future-form", true), None);
    assert_eq!(resolve_decision_action("future-form", false), Some("deny"));
    let response = super::decision::decision_response_for("continue", String::new());
    assert_eq!(response.action, "continue");
    assert!(response.data.is_empty(), "url responses carry no data");
}

#[tokio::test]
async fn url_modal_enter_after_idle_gate_starts_submission() {
    let mut entries = requests();
    ready(&mut entries);
    let mut pending = pending_confirmation(&entries).unwrap().unwrap();
    pending.kind = "url".into();
    pending.payload = json!({"url": "https://example.com/connect/abc", "message": "Connect Jira"});
    let config = crate::tests::test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.modal = None;
    tui.interactions.target = Some(("origin-session".into(), "origin-cluster".into()));
    snapshot(&mut tui, Some(pending));
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    // A fresh keypress cannot accidentally continue (same idle gate as confirm).
    tui.handle_interaction_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .await;
    tui.handle_interaction_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert!(!modal(&mut tui).submitting, "idle gate must hold");
    // After the gate, Enter submits continue.
    modal(&mut tui).last_key_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
    tui.handle_interaction_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert!(modal(&mut tui).submitting, "Enter past the gate continues");
}

#[test]
fn middleware_notes_are_response_fields_not_user_messages() {
    let deny = decision_response(false, "leave it alone\nplease".into());
    assert_eq!(deny.action, "deny");
    assert_eq!(deny.note.as_deref(), Some("leave it alone\nplease"));
    assert!(deny.data.is_empty());
    assert_eq!(decision_response(true, " \n ".into()).note, None);
}
