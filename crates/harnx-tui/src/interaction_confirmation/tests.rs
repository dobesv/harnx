use super::decision::decision_response;
use super::*;
use harnx_core::session::SessionLogEntry;
use serde_json::json;
mod a2ui;
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

#[test]
fn middleware_notes_are_response_fields_not_user_messages() {
    let deny = decision_response(false, "leave it alone\nplease".into());
    assert_eq!(deny.action, "deny");
    assert_eq!(deny.note.as_deref(), Some("leave it alone\nplease"));
    assert!(deny.data.is_empty());
    assert_eq!(decision_response(true, " \n ".into()).note, None);
}
