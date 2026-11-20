//! Unit tests for `harnx interact` — rendering, identity, response building,
//! and classification of answered re-runs.

use super::answer::{answer_target, build_response};
use super::rerun::answered_request;
use super::{pending_text, SessionInteractions};
use crate::cli::InteractAnswerArgs;
use harnx_core::interaction::InteractionRequest;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::interactions::PendingInteraction;
use serde_json::{json, Map, Value};

fn a2ui_payload() -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "dataModel": {"choice": ["staging"], "name": "rel-1"},
            "components": [
                {"id": "root", "component": "Card", "child": "col"},
                {"id": "col", "component": "Column",
                 "children": ["title", "choice", "name", "go_label", "go"]},
                {"id": "title", "component": "Text", "text": "Pick a deployment target"},
                {"id": "choice", "component": "ChoicePicker",
                 "options": [{"label": "Staging", "value": "staging"},
                             {"label": "Production", "value": "prod"}],
                 "value": {"@path": "/choice"}},
                {"id": "name", "component": "TextField", "label": "Release name",
                 "value": {"@path": "/name"}},
                {"id": "go_label", "component": "Text", "text": "Deploy"},
                {"id": "go", "component": "Button", "child": "go_label",
                 "action": {"event": {"name": "deploy"}}}
            ]
        }
    })
}

// qual:allow(dry, duplicate) reason: "test-only PendingInteraction builder; harnx-acp-server has an identical one, and the crates share no test utilities"
fn pending(kind: &str, payload: Value) -> PendingInteraction {
    PendingInteraction {
        request_seq: 3,
        tool_round_seq: 1,
        tool_call_id: "call-1".into(),
        interaction_id: "int-form".into(),
        request: InteractionRequest {
            interaction_id: "int-form".into(),
            producer: "middleware:test".into(),
            step_path: vec![],
            key: "form".into(),
            kind: kind.into(),
            payload,
            summary: "Pick a deployment target".into(),
            chain_version: "v1".into(),
        },
    }
}

fn answer_args(action: &str, data: &[&str]) -> InteractAnswerArgs {
    InteractAnswerArgs {
        agent: "agent@fixture".into(),
        session: "sess".into(),
        interaction_id: "int-form".into(),
        request_seq: None,
        action: action.into(),
        data: data.iter().map(|pair| pair.to_string()).collect(),
        data_json: None,
        note: Some("ship it".into()),
    }
}

#[test]
fn a2ui_answer_builds_canonical_data_with_defaults_and_note() {
    let target = pending("a2ui", a2ui_payload());
    let args = answer_args("deploy", &["choice=Production", "/name=release 2"]);
    let response =
        build_response(&target, &args.action, &args.data, None, args.note.clone()).unwrap();
    assert_eq!(response.action, "deploy");
    assert_eq!(
        response.data,
        Map::from_iter([
            ("choice".into(), json!(["prod"])),
            ("name".into(), json!("release 2")),
        ])
    );
    assert_eq!(response.note.as_deref(), Some("ship it"));
}

#[test]
fn a2ui_deny_carries_no_data() {
    let target = pending("a2ui", a2ui_payload());
    let response = build_response(&target, "deny", &[], None, None).unwrap();
    assert_eq!(response.action, "deny");
    assert!(response.data.is_empty());
}

#[test]
fn a2ui_untouched_inputs_use_declared_defaults() {
    let target = pending("a2ui", a2ui_payload());
    let response = build_response(&target, "deploy", &[], None, None).unwrap();
    assert_eq!(
        response.data,
        Map::from_iter([
            ("choice".into(), json!(["staging"])),
            ("name".into(), json!("rel-1")),
        ])
    );
}

#[test]
fn a2ui_data_json_supplies_typed_values() {
    let target = pending("a2ui", a2ui_payload());
    let response = build_response(
        &target,
        "deploy",
        &[],
        Some(r#"{"choice": ["prod"], "name": "rel-2"}"#),
        None,
    )
    .unwrap();
    assert_eq!(
        response.data,
        Map::from_iter([
            ("choice".into(), json!(["prod"])),
            ("name".into(), json!("rel-2")),
        ])
    );
}

#[test]
fn a2ui_invalid_answers_are_rejected_before_submission() {
    let target = pending("a2ui", a2ui_payload());
    let undeclared = build_response(&target, "rollback", &[], None, None)
        .unwrap_err()
        .to_string();
    assert!(undeclared.contains("not declared"), "{undeclared}");
    let bad_option = build_response(
        &target,
        "deploy",
        &["choice=canary".to_string()],
        None,
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(bad_option.contains("canary"), "{bad_option}");
    let bad_key = build_response(&target, "deploy", &["bogus=1".to_string()], None, None)
        .unwrap_err()
        .to_string();
    assert!(bad_key.contains("unknown input key 'bogus'"), "{bad_key}");
    let bad_json_key = build_response(&target, "deploy", &[], Some(r#"{"nope": 1}"#), None)
        .unwrap_err()
        .to_string();
    assert!(
        bad_json_key.contains("unknown input key 'nope'"),
        "{bad_json_key}"
    );
}

#[test]
fn confirm_accepts_only_approve_and_deny() {
    let target = pending("confirm", json!({"message": "Deploy?"}));
    for action in ["approve", "deny"] {
        let response = build_response(&target, action, &[], None, None).unwrap();
        assert_eq!(response.action, action);
        assert!(response.data.is_empty());
    }
    let error = build_response(&target, "deploy", &[], None, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("only 'approve' or 'deny'"), "{error}");
}

#[test]
fn unsupported_kinds_are_rejected() {
    let target = pending("secret", json!({}));
    let error = build_response(&target, "deny", &[], None, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot be answered from the CLI"), "{error}");
}

#[test]
fn answer_target_requires_exact_identity() {
    let mut other = pending("a2ui", a2ui_payload());
    other.interaction_id = "int-other".into();
    other.request.interaction_id = "int-other".into();
    other.request_seq = 9;
    // Single match by id.
    let list = vec![pending("a2ui", a2ui_payload()), other];
    assert_eq!(
        answer_target(&list, "int-other", None).unwrap().request_seq,
        9
    );
    assert_eq!(
        answer_target(&list, "int-form", Some(3))
            .unwrap()
            .request_seq,
        3
    );
    // Sequence mismatch is an error, never "answer whatever is pending".
    let error = answer_target(&list, "int-form", Some(42))
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match"), "{error}");
    // Unknown id.
    let error = answer_target(&list, "int-missing", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not pending"), "{error}");
    // Multiple pending requests sharing one id require the sequence.
    let shared = vec![
        pending("a2ui", a2ui_payload()),
        PendingInteraction {
            request_seq: 9,
            ..pending("a2ui", a2ui_payload())
        },
    ];
    let error = answer_target(&shared, "int-form", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("--request-seq"), "{error}");
    assert_eq!(
        answer_target(&shared, "int-form", Some(9))
            .unwrap()
            .request_seq,
        9
    );
}

#[test]
fn answered_rerun_classifies_identical_and_conflicting() {
    // History: one answered a2ui request plus its recorded response.
    let request = SessionLogEntry::InteractionRequested {
        tool_round_seq: 1,
        tool_call_id: "call-1".into(),
        interaction_id: "int-form".into(),
        producer: "middleware:test".into(),
        step_path: vec![],
        key: "form".into(),
        kind: "a2ui".into(),
        payload: a2ui_payload(),
        summary: "Pick a deployment target".into(),
        chain_version: "v1".into(),
        checkpoint: "cp".into(),
        fence_token: 10,
    };
    let recorded = harnx_core::interaction::InteractionResponse {
        action: "deploy".into(),
        data: Map::from_iter([
            ("choice".into(), json!(["prod"])),
            ("name".into(), json!("rel-2")),
        ]),
        note: Some("ship it".into()),
    };
    let history = vec![
        (1, request),
        (
            2,
            SessionLogEntry::InteractionResponse {
                request_seq: 1,
                interaction_id: "int-form".into(),
                tool_call_id: "call-1".into(),
                response: recorded.clone(),
                fence_token: 10,
            },
        ),
    ];
    let snapshot = SessionInteractions {
        pending: Vec::new(),
        history,
    };

    // The flags that produced the recorded response rebuild it exactly.
    let (target, recorded) =
        answered_request(&snapshot.history, "int-form", None).expect("answered");
    let response = build_response(
        &target,
        "deploy",
        &["choice=Production".to_string(), "name=rel-2".to_string()],
        None,
        Some("ship it".into()),
    )
    .unwrap();
    assert_eq!(response, *recorded, "identical re-run must match");
    // A different answer must not match.
    let different = build_response(&target, "deny", &[], None, None).unwrap();
    assert_ne!(different, *recorded);
    // An unanswered id finds nothing.
    assert!(answered_request(&snapshot.history, "int-missing", None).is_none());
}

#[test]
fn pending_text_renders_the_surface() {
    let item = pending("a2ui", a2ui_payload());
    let text = pending_text(&item);
    assert!(text.contains("Pending interaction int-form (kind a2ui, request sequence 3)"));
    assert!(text.contains("Pick a deployment target"));
    assert!(text.contains("Release name"));
    assert!(text.contains("deploy"));
    let confirm = pending("confirm", json!({"message": "Deploy?"}));
    let text = pending_text(&confirm);
    assert!(text.contains("Deploy?"));
    assert!(!text.contains("cannot be answered from the CLI"));
}
