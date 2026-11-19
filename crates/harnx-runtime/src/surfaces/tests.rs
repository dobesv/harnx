use super::*;
use harnx_core::interaction::non_modal::SurfaceVisibility;
use harnx_core::session::ToolOutput;
use serde_json::json;

fn surface_payload(id: &str) -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": id,
            "components": [
                { "id": "title", "component": "Text", "text": "Deploy?" },
                { "id": "go", "component": "Button", "text": "Go",
                  "action": { "event": { "name": "deploy" } } }
            ]
        }
    })
}

fn results_entry(seq: u64, surfaces: Vec<Value>) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("call-1".into()),
                name: "deployer".into(),
                output: json!({}),
                markdown: None,
                content: Vec::new(),
                user_surfaces: surfaces,
                switch_agent: None,
            }],
            handoff_selection: None,
            timestamp: None,
        },
    )
}

fn submission(action: &str) -> SurfaceActionSubmission {
    SurfaceActionSubmission {
        surface_id: "s-1".into(),
        publish_seq: 7,
        response: InteractionResponse {
            action: action.into(),
            data: serde_json::Map::new(),
            note: None,
        },
        user_message: None,
    }
}

#[test]
fn finds_published_surface_by_id_and_seq() {
    let entries = vec![
        results_entry(7, vec![surface_payload("s-1")]),
        results_entry(8, vec![surface_payload("s-2")]),
    ];
    let found = find_published_surface(&entries, "s-1", 7).unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().tool_call_id.as_deref(), Some("call-1"));
    assert!(find_published_surface(&entries, "s-1", 8)
        .unwrap()
        .is_none());
    assert!(find_published_surface(&entries, "s-9", 7)
        .unwrap()
        .is_none());
}

#[test]
fn validates_submission_against_the_published_generation() {
    let entries = vec![results_entry(7, vec![surface_payload("s-1")])];
    let declaration = validate_surface_submission(&entries, &submission("deploy")).unwrap();
    // No `harnx` extension on the button: the default for a tool-less
    // action is `turn`.
    assert_eq!(declaration.visibility, SurfaceVisibility::Turn);
    assert!(declaration.tool.is_none());

    assert_eq!(
        validate_surface_submission(&entries, &submission("nope")).unwrap_err(),
        SurfaceActionOutcome::UndeclaredAction
    );
    assert_eq!(
        validate_surface_submission(&entries, &{
            let mut s = submission("deploy");
            s.publish_seq = 99;
            s
        })
        .unwrap_err(),
        SurfaceActionOutcome::UnknownSurface
    );
}

#[test]
fn first_wins_existing_action_conflicts() {
    let entries = vec![
        results_entry(7, vec![surface_payload("s-1")]),
        (
            9,
            SessionLogEntry::SurfaceAction {
                surface_id: "s-1".into(),
                publish_seq: 7,
                action: "deploy".into(),
                data: json!({"env": "prod"}),
                note: None,
                user_message: None,
                visibility: "note".into(),
                note_text: Some("deployed".into()),
                fence_token: 1,
            },
        ),
    ];
    let base = submission("deploy");
    let identical = SurfaceActionSubmission {
        response: InteractionResponse {
            action: "deploy".into(),
            data: serde_json::from_value(json!({"env": "prod"})).unwrap(),
            note: None,
        },
        ..base.clone()
    };
    let conflicting = SurfaceActionSubmission {
        response: InteractionResponse {
            data: serde_json::from_value(json!({"env": "stage"})).unwrap(),
            ..identical.response.clone()
        },
        ..base
    };
    assert_eq!(
        existing_surface_action(&entries, "s-1", 7)
            .map(|accepted| (accepted.action, accepted.data)),
        Some(("deploy".into(), json!({"env": "prod"})))
    );
    assert_eq!(
        validate_surface_submission(&entries, &identical).unwrap_err(),
        SurfaceActionOutcome::AlreadyApplied
    );
    assert_eq!(
        validate_surface_submission(&entries, &conflicting).unwrap_err(),
        SurfaceActionOutcome::Conflicting
    );
}

#[test]
fn fold_skips_delivered_notes_and_other_visibilities() {
    let note = |seq: u64, id: &str, note_text: &str| {
        (
            seq,
            SessionLogEntry::SurfaceAction {
                surface_id: id.into(),
                publish_seq: 7,
                action: "deploy".into(),
                data: json!({}),
                note: None,
                user_message: None,
                visibility: "note".into(),
                note_text: Some(note_text.into()),
                fence_token: 1,
            },
        )
    };
    let turn_end = |seq: u64| {
        (
            seq,
            SessionLogEntry::TurnEnd {
                through_seq: seq,
                fence_token: 1,
                timestamp: None,
                usage: None,
            },
        )
    };
    // The hook-context row a mid-turn delivery wrote for the first note.
    let delivered_row = |seq: u64, id: &str, text: &str| {
        (
            seq,
            SessionLogEntry::Message {
                id: Some(id.into()),
                role: harnx_core::message::MessageRole::User,
                content: harnx_core::message::MessageContent::Text(text.into()),
                timestamp: None,
                fence_token: None,
            },
        )
    };
    let hook_id = harnx_core::runtime_note::new_hook_context_message_id();
    let entries = vec![
        results_entry(7, vec![surface_payload("s-1")]),
        note(8, "s-1", "delivered mid-turn"),
        delivered_row(9, &hook_id, "delivered mid-turn"),
        turn_end(10),
        note(11, "s-1", "idle note one"),
        (
            12,
            SessionLogEntry::SurfaceAction {
                surface_id: "s-1".into(),
                publish_seq: 7,
                action: "later".into(),
                data: json!({}),
                note: None,
                user_message: None,
                visibility: "invisible".into(),
                note_text: None,
                fence_token: 1,
            },
        ),
        note(13, "s-1", "idle note two"),
    ];
    assert_eq!(
        fold_pending_surface_notes(&entries).as_deref(),
        Some("idle note one\n\nidle note two")
    );
    // Before the delivery row exists the note is still pending, so the
    // next model call folds it; the row at seq 9 is what retires it.
    assert_eq!(
        fold_pending_surface_notes(&entries[..2]).as_deref(),
        Some("delivered mid-turn")
    );
}
