use super::{drawn_screen, modal, snapshot, tui_with_pending};
use crate::interaction_confirmation::a2ui::{A2UIFormState, FocusTarget, FormField};
use crate::interaction_confirmation::{InteractionEvent, InteractionKey, ModalState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::interaction::response::build_data;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::interactions::InteractionDecisionOutcome;
use serde_json::{json, Value};

pub(super) fn sample_a2ui_payload() -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "dataModel": { "choice": ["staging"], "name": "release-1", "region": "us-east" },
            "components": [
                { "id": "root", "component": "Card", "child": "col" },
                { "id": "col", "component": "Column", "children": ["title", "choice", "name", "submit_label", "submit"] },
                { "id": "title", "component": "Text", "text": "Pick deployment target" },
                {
                    "id": "choice", "component": "ChoicePicker", "variant": "mutuallyExclusive",
                    "options": [{ "label": "Staging", "value": "staging" }, { "label": "Production", "value": "prod" }],
                    "value": { "@path": "/choice" }
                },
                { "id": "name", "component": "TextField", "label": "Release Name", "value": { "@path": "/name" } },
                { "id": "submit_label", "component": "Text", "text": "Deploy" },
                {
                    "id": "submit", "component": "Button", "variant": "primary", "child": "submit_label",
                    "action": {
                        "event": {
                            "name": "deploy",
                            "context": { "choice": { "@path": "/choice" }, "mode": "fast", "region": { "@path": "/region" } }
                        }
                    }
                }
            ]
        }
    })
}

fn a2ui_entry(seq: u64, id: &str, payload: Value) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::InteractionRequested {
            tool_round_seq: 1,
            tool_call_id: format!("call-{id}"),
            interaction_id: id.into(),
            producer: "middleware:test".into(),
            step_path: vec![],
            key: id.into(),
            kind: "a2ui".into(),
            payload,
            summary: "A2UI interaction".into(),
            chain_version: "v1".into(),
            checkpoint: "private".into(),
            fence_token: 1,
        },
    )
}

pub(super) fn a2ui_round_entries(id: &str, payload: Value) -> Vec<(u64, SessionLogEntry)> {
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
        a2ui_entry(2, id, payload),
        (
            3,
            SessionLogEntry::SuspensionReady {
                tool_round_seq: 1,
                pending: vec![id.into()],
                fence_token: 1,
            },
        ),
    ]
}

#[tokio::test]
async fn a2ui_form_state_initialization_and_shared_fixture_parity() {
    let fixture_content =
        std::fs::read_to_string("../harnx-core/tests/fixtures/a2ui-build-data-cases.json")
            .or_else(|_| {
                std::fs::read_to_string(
                    "crates/harnx-core/tests/fixtures/a2ui-build-data-cases.json",
                )
            })
            .expect("read a2ui-build-data-cases.json");
    let cases: Vec<Value> = serde_json::from_str(&fixture_content).unwrap();

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let surface = &case["surface"];
        let action = case["action"].as_str().unwrap();
        let expected = &case["expected"];

        let form = A2UIFormState::new(surface);
        assert!(
            form.is_some(),
            "Form state creation failed for case: {name}"
        );
        let form = form.unwrap();

        let values = form.values();
        let built = build_data(surface, action, &values).expect("build_data succeeded");
        if action != "deny" && name.contains("defaults untouched") {
            assert_eq!(Value::Object(built), *expected, "Parity failed for: {name}");
        }
    }
}

#[tokio::test]
async fn a2ui_editing_and_focus_navigation() {
    let payload = sample_a2ui_payload();
    let mut form = A2UIFormState::new(&payload).unwrap();

    assert_eq!(form.focus, FocusTarget::Field(0));
    match &form.fields[0] {
        FormField::Choice(cp) => {
            assert_eq!(cp.selected, vec!["staging"]);
            assert_eq!(cp.highlighted, 0);
        }
        _ => panic!("expected choice picker at field 0"),
    }

    form.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    form.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    match &form.fields[0] {
        FormField::Choice(cp) => assert_eq!(cp.selected, vec!["prod"]),
        _ => panic!("expected choice picker"),
    }

    form.focus_next();
    assert_eq!(form.focus, FocusTarget::Field(1));

    form.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    form.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
    form.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    form.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    match &form.fields[1] {
        FormField::Text(tf) => assert_eq!(tf.value, "release-1-v2"),
        _ => panic!("expected text field"),
    }

    form.focus_next();
    assert_eq!(form.focus, FocusTarget::Button(0));
    form.focus_next();
    assert_eq!(form.focus, FocusTarget::Note);
    form.focus_next();
    assert_eq!(form.focus, FocusTarget::Field(0));
}

#[tokio::test]
async fn a2ui_modal_renders_in_tui_and_submits_primary_action() {
    let mut tui = tui_with_pending().await;
    let payload = sample_a2ui_payload();
    let mut entries = a2ui_round_entries("a2ui-test", payload);
    let pending = super::pending_confirmation(&entries).unwrap().unwrap();
    assert_eq!(pending.kind, "a2ui");
    assert!(!pending.payload.is_null());
    snapshot(&mut tui, Some(pending));

    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmInteraction(_))
    ));
    let mut state = modal(&mut tui).clone();
    assert!(state.a2ui.is_some());

    let screen = drawn_screen(&mut tui, &mut state, 100, 20);
    assert!(screen.contains("Pick deployment target"));
    assert!(screen.contains("Staging"));
    assert!(screen.contains("Production"));
    assert!(screen.contains("Release Name"));
    assert!(screen.contains("Deploy"));
    assert!(screen.contains("Tab navigate"));

    tui.handle_interaction_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    tui.handle_paste("my-app".into()).await;
    tui.submit_interaction(true);

    assert!(modal(&mut tui).submitting);
    assert_eq!(tui.interactions.decisions.len(), 1);

    let key = InteractionKey {
        id: "a2ui-test".into(),
        request_seq: 2,
    };
    tui.handle_interaction_event(InteractionEvent::Decided {
        generation: tui.interactions.generation,
        target: tui.interactions.target.clone().unwrap(),
        key: key.clone(),
        result: Ok(InteractionDecisionOutcome::InvalidResponse),
    });

    let state = modal(&mut tui);
    assert!(!state.submitting);
    assert!(state.error.as_ref().unwrap().contains("invalid_response"));
    assert!(tui.app.modal.is_some());

    entries.push((
        4,
        SessionLogEntry::InteractionResponse {
            request_seq: 2,
            interaction_id: "a2ui-test".into(),
            tool_call_id: "call-a2ui-test".into(),
            response: harnx_core::interaction::InteractionResponse {
                action: "deploy".into(),
                data: Default::default(),
                note: None,
            },
            fence_token: 1,
        },
    ));
    snapshot(&mut tui, super::pending_confirmation(&entries).unwrap());
    assert!(tui.app.modal.is_none());
}

#[tokio::test]
async fn a2ui_unsupported_component_falls_back_to_deny_only() {
    let mut tui = tui_with_pending().await;
    let invalid_payload = json!({
        "version": "1.0",
        "createSurface": {
            "components": [{ "id": "root", "component": "Slider", "min": 0, "max": 10 }]
        }
    });

    let entries = a2ui_round_entries("bad-a2ui", invalid_payload);
    let pending = super::pending_confirmation(&entries).unwrap().unwrap();
    snapshot(&mut tui, Some(pending));

    let mut state = modal(&mut tui).clone();
    assert!(state.a2ui.is_none());
    assert_eq!(state.pending.kind, "a2ui");

    let screen = drawn_screen(&mut tui, &mut state, 100, 10);
    assert!(screen.contains("not supported by this client. Deny only."));
}

#[tokio::test]
async fn a2ui_deny_submits_empty_data_with_note() {
    let mut tui = tui_with_pending().await;
    let payload = sample_a2ui_payload();
    let entries = a2ui_round_entries("deny-test", payload);
    let pending = super::pending_confirmation(&entries).unwrap().unwrap();
    snapshot(&mut tui, Some(pending));

    modal(&mut tui).note.insert_str("Not now");
    tui.handle_interaction_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .await;
    assert!(modal(&mut tui).submitting);
    assert_eq!(tui.interactions.decisions.len(), 1);
}
