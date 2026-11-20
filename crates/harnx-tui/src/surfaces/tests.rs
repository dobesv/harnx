//! Tests for the TUI non-modal surface frontend (plan T5.1 Part B):
//! durable-snapshot hydration, transcript rows, form opening, submission
//! state and outcome handling — all without blocking the composer.

use super::*;
use crate::types::{ModalState, TranscriptItem};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::surfaces::SurfaceActionOutcome;
use serde_json::json;

/// A valid single-surface envelope with one TextField, one ChoicePicker and
/// one Button, shaped like the Part A integration fixtures.
fn envelope(id: &str) -> serde_json::Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": id,
            "dataModel": { "name": "", "env": [] },
            "components": [
                { "id": "root", "component": "Card", "child": "col" },
                { "id": "col", "component": "Column",
                 "children": ["title", "name", "env", "ship_label", "ship"] },
                { "id": "title", "component": "Text", "text": "Deployment ready" },
                { "id": "name", "component": "TextField", "label": "Release name",
                 "value": { "@path": "/name" } },
                { "id": "env", "component": "ChoicePicker",
                 "options": [
                    { "label": "Production", "value": "prod" },
                    { "label": "Staging", "value": "staging" }
                 ], "value": { "@path": "/env" } },
                { "id": "ship_label", "component": "Text", "text": "Ship it" },
                { "id": "ship", "component": "Button", "child": "ship_label",
                 "action": { "event": { "name": "ship" } } }
            ]
        }
    })
}

fn snapshot(id: &str, seq: u64, action: Option<&str>) -> SurfaceSnapshot {
    SurfaceSnapshot {
        surface_id: id.into(),
        publish_seq: seq,
        payload: envelope(id),
        action: action.map(str::to_string),
    }
}

fn feed(tui: &mut Tui, snapshots: Vec<SurfaceSnapshot>) {
    tui.handle_surface_event(SurfaceEvent::Snapshot {
        generation: tui.surfaces.generation,
        surfaces: Ok(snapshots),
    });
}

fn decided(tui: &mut Tui, surface_id: &str, result: Result<SurfaceActionOutcome, String>) {
    tui.handle_surface_event(SurfaceEvent::Decided {
        generation: tui.surfaces.generation,
        surface_id: surface_id.into(),
        result,
    });
}

async fn tui_with_surface() -> Tui {
    let config = crate::tests::test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.surfaces.target = Some(("origin-session".into(), "origin-cluster".into()));
    feed(&mut tui, vec![snapshot("deploy-1", 3, None)]);
    tui
}

/// The focused surface row index, failing loudly if the row is missing.
fn focus_surface_row(tui: &mut Tui) -> usize {
    tui.app
        .transcript
        .iter()
        .position(|item| matches!(item, TranscriptItem::UserSurface { .. }))
        .expect("surface row in transcript")
}

#[tokio::test]
async fn snapshot_appends_transcript_rows_and_marks_resolution() {
    let mut tui = tui_with_surface().await;
    assert!(tui.surfaces.snapshots.contains_key("deploy-1"));
    let index = focus_surface_row(&mut tui);
    assert!(matches!(
        &tui.app.transcript[index],
        TranscriptItem::UserSurface { resolved: None, .. }
    ));

    // Resolution can arrive from any frontend; the row refreshes in place.
    feed(&mut tui, vec![snapshot("deploy-1", 3, Some("ship"))]);
    let index = focus_surface_row(&mut tui);
    let TranscriptItem::UserSurface { resolved, .. } = &tui.app.transcript[index] else {
        panic!("surface row moved");
    };
    assert_eq!(resolved.as_deref(), Some("ship"));
}

#[tokio::test]
async fn durable_entries_hydrate_resolution_for_history_rebuild() {
    let publish = SessionLogEntry::ToolResults {
        results: vec![harnx_core::session::ToolOutput {
            id: Some("tool-1".into()),
            name: "publish".into(),
            output: json!({}),
            markdown: None,
            content: Vec::new(),
            user_surfaces: vec![envelope("deploy-1")],
            switch_agent: None,
        }],
        handoff_selection: None,
        timestamp: None,
    };
    let action = SessionLogEntry::SurfaceAction {
        surface_id: "deploy-1".into(),
        publish_seq: 3,
        action: "ship".into(),
        data: json!({}),
        note: None,
        user_message: None,
        visibility: "invisible".into(),
        note_text: None,
        fence_token: 7,
    };
    let hydrated =
        SurfaceSnapshot::from_entries(&[(3, publish), (9, action)]).expect("snapshots derive");
    assert_eq!(hydrated.len(), 1);
    assert_eq!(hydrated[0].action.as_deref(), Some("ship"));
    assert_eq!(hydrated[0].publish_seq, 3);
}

#[tokio::test]
async fn open_refuses_without_snapshot_and_when_resolved() {
    let mut tui = tui_with_surface().await;

    // Outstanding surface with a durable snapshot opens the action form.
    let index = focus_surface_row(&mut tui);
    tui.app.transcript_focus = Some(index);
    assert!(tui.open_focused_root_surface());
    assert!(matches!(tui.app.modal, Some(ModalState::SurfaceAction(_))));
    let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_ref() else {
        panic!("modal missing");
    };
    assert!(state.a2ui.is_some(), "valid envelope builds the A2UI form");

    // A second focused surface cannot open while the form is up (explicit
    // modal, one form at a time), but the first form stays intact.
    tui.app.modal = None;
    let resolved = snapshot("deploy-2", 5, Some("ship"));
    feed(&mut tui, vec![snapshot("deploy-1", 3, None), resolved]);
    let index = tui
        .app
        .transcript
        .iter()
        .position(|item| matches!(item, TranscriptItem::UserSurface { surface_id, resolved: Some(_), .. } if surface_id == "deploy-2"))
        .expect("resolved row");
    tui.app.transcript_focus = Some(index);
    assert!(tui.open_focused_root_surface());
    assert!(
        tui.app.modal.is_none(),
        "resolved surface cannot be re-answered"
    );

    // A row whose durable snapshot has not arrived yet refuses politely.
    tui.push_orphan_surface_row();
    let index = tui
        .app
        .transcript
        .iter()
        .position(|item| matches!(item, TranscriptItem::UserSurface { surface_id, .. } if surface_id == "ghost-1"))
        .expect("ghost row");
    tui.app.transcript_focus = Some(index);
    assert!(tui.open_focused_root_surface());
    assert!(tui.app.modal.is_none(), "no snapshot, no form");
}

impl Tui {
    /// Test helper: a surface row with no durable snapshot (history rebuild
    /// raced the observer).
    fn push_orphan_surface_row(&mut self) {
        self.app.transcript.push(TranscriptItem::UserSurface {
            surface_id: "ghost-1".into(),
            payload: envelope("ghost-1"),
            resolved: None,
        });
    }
}

/// Plain-text view of one rendered transcript row, for assertions.
fn entry_text(entry: &crate::markdown_render::RenderedEntry) -> String {
    use crate::markdown_render::MarkdownBlockData;
    let mut text = String::new();
    for block in &entry.blocks {
        if let MarkdownBlockData::Paragraph { lines, .. } = block {
            for line in lines {
                for span in &line.spans {
                    text.push_str(span.content.as_ref());
                }
            }
        }
    }
    text
}

/// One surface row for rendering assertions, with its resolution.
fn surface_row(resolved: Option<&str>) -> TranscriptItem {
    TranscriptItem::UserSurface {
        surface_id: "deploy-1".into(),
        payload: envelope("deploy-1"),
        resolved: resolved.map(str::to_string),
    }
}

#[tokio::test]
async fn row_rendering_distinguishes_outstanding_and_answered() {
    let outstanding = surface_row(None);
    let text = entry_text(&Tui::render_surface_row(&outstanding, 80));
    assert!(text.contains("Deployment ready"), "{text}");
    assert!(text.contains("Enter to act"), "{text}");

    let answered = surface_row(Some("ship"));
    let text = entry_text(&Tui::render_surface_row(&answered, 80));
    assert!(text.contains("answered: ship"), "{text}");
    assert!(!text.contains("Enter to act"), "{text}");
}

#[tokio::test]
async fn submit_marks_row_on_success_and_keeps_form_open_on_rejection() {
    let mut tui = tui_with_surface().await;
    let index = focus_surface_row(&mut tui);
    tui.app.transcript_focus = Some(index);
    assert!(tui.open_focused_root_surface());

    // An undeclared action fails data building before any submission.
    tui.submit_surface_action("self_destruct".into());
    let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_ref() else {
        panic!("form missing");
    };
    assert!(!state.submitting);
    assert!(state
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("not declared"));

    // A valid submit spawns the durable task and never blocks the composer.
    tui.submit_surface_action("ship".into());
    let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_ref() else {
        panic!("form missing");
    };
    assert!(state.submitting, "submission is in flight");
    tui.set_input_text("composer still works");
    assert_eq!(tui.app.input.lines().join("\n"), "composer still works");

    // Rejection keeps the form open with the reason; the row stays outstanding.
    decided(&mut tui, "deploy-1", Ok(SurfaceActionOutcome::Conflicting));
    let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_ref() else {
        panic!("form must stay open on rejection");
    };
    assert!(!state.submitting);
    assert!(state
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("conflicting"));
    let index = focus_surface_row(&mut tui);
    assert!(matches!(
        &tui.app.transcript[index],
        TranscriptItem::UserSurface { resolved: None, .. }
    ));

    // A retry after a rejection is in flight again; success closes the form.
    tui.submit_surface_action("ship".into());
    let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_ref() else {
        panic!("form missing before retry");
    };
    assert!(state.submitting, "retry is in flight");
    decided(&mut tui, "deploy-1", Ok(SurfaceActionOutcome::Applied));
    assert!(tui.app.modal.is_none(), "form closes on success");
    let index = focus_surface_row(&mut tui);
    let TranscriptItem::UserSurface { resolved, .. } = &tui.app.transcript[index] else {
        panic!("surface row moved");
    };
    assert_eq!(resolved.as_deref(), Some("answered"));

    // The resolved surface refuses to be re-opened (first-wins).
    tui.app.transcript_focus = Some(index);
    assert!(tui.open_focused_root_surface());
    assert!(tui.app.modal.is_none());
}

#[tokio::test]
async fn esc_and_ctrl_c_are_swallowed_while_surface_submission_runs() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut tui = tui_with_surface().await;
    let index = focus_surface_row(&mut tui);
    tui.app.transcript_focus = Some(index);
    assert!(tui.open_focused_root_surface());

    tui.submit_surface_action("ship".into());
    let decisions_before = tui.surfaces.decisions.len();
    assert_eq!(decisions_before, 1, "one submission task should be running");

    for key in [
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    ] {
        tui.handle_surface_key(key);
        let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_ref() else {
            panic!("form must stay open while submission runs");
        };
        assert!(state.submitting, "keys are swallowed while submitting");
    }

    // The public submit path also refuses a second call while this form is busy.
    tui.submit_surface_action("ship".into());
    assert_eq!(tui.surfaces.decisions.len(), decisions_before);
}

#[test]
fn surface_decision_errors_cover_append_failure_and_pending_outcomes() {
    assert_eq!(
        surface_decision_error(&Ok(SurfaceActionOutcome::AppendFailed)).as_deref(),
        Some(
            "append_failed: a handler panic or record failure left the action outcome \
             uncertain; a direct tool may have run. Check the session before retrying."
        )
    );
    assert_eq!(
        surface_decision_error(&Ok(SurfaceActionOutcome::Pending)).as_deref(),
        Some("pending: the action is still being processed; check its outcome before retrying.")
    );
    assert_eq!(
        surface_decision_error(&Ok(SurfaceActionOutcome::LeaseLost)).as_deref(),
        Some(
            "lease_lost: the action outcome is uncertain; a direct tool may have run. \
             Check the session before retrying."
        )
    );
}

#[tokio::test]
async fn stale_generation_decisions_are_ignored() {
    let mut tui = tui_with_surface().await;
    let index = focus_surface_row(&mut tui);
    tui.app.transcript_focus = Some(index);
    assert!(tui.open_focused_root_surface());

    // A decision from a previous session target (older generation) must not
    // touch the current form or rows.
    tui.handle_surface_event(SurfaceEvent::Decided {
        generation: tui.surfaces.generation.wrapping_add(3),
        surface_id: "deploy-1".into(),
        result: Ok(SurfaceActionOutcome::Applied),
    });
    let Some(ModalState::SurfaceAction(_)) = tui.app.modal.as_ref() else {
        panic!("form must be untouched by a stale decision");
    };
    let index = focus_surface_row(&mut tui);
    assert!(matches!(
        &tui.app.transcript[index],
        TranscriptItem::UserSurface { resolved: None, .. }
    ));
}

#[tokio::test]
async fn surface_headline_uses_first_text_component() {
    assert_eq!(surface_headline(&envelope("deploy-1")), "Deployment ready");
    let bare = json!({ "createSurface": { "surfaceId": "quiet-1", "components": [] } });
    assert_eq!(surface_headline(&bare), "quiet-1");
}

#[tokio::test]
async fn lowercase_c_is_text_in_fields_and_note_while_esc_closes() {
    use crate::interaction_confirmation::a2ui::{FocusTarget, FormField};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut tui = tui_with_surface().await;
    let index = focus_surface_row(&mut tui);
    tui.app.transcript_focus = Some(index);
    assert!(tui.open_focused_root_surface());

    if let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_mut() {
        let form = state.a2ui.as_mut().expect("form available");
        form.focus = FocusTarget::Field(0);
    } else {
        panic!("surface form should be open");
    }
    for character in "config".chars() {
        tui.handle_surface_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        assert!(matches!(tui.app.modal, Some(ModalState::SurfaceAction(_))));
    }
    let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_mut() else {
        panic!("typing in field must keep form open");
    };
    let Some(FormField::Text(field)) = state.a2ui.as_ref().unwrap().fields.first() else {
        panic!("first field should be text");
    };
    assert_eq!(field.value, "config");

    state.a2ui.as_mut().unwrap().focus = FocusTarget::Note;
    for character in "config".chars() {
        tui.handle_surface_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        assert!(matches!(tui.app.modal, Some(ModalState::SurfaceAction(_))));
    }
    let Some(ModalState::SurfaceAction(state)) = tui.app.modal.as_ref() else {
        panic!("typing in note must keep form open");
    };
    assert_eq!(state.note.lines().join("\n"), "config");

    tui.handle_surface_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(tui.app.modal.is_none(), "Esc closes the surface form");
}
