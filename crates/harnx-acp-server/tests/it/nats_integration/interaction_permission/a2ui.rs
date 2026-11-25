//! ACP answers general `a2ui` interactions through the shared text fallback.

use agent_client_protocol::v1::schema::{PermissionOptionKind, ToolCallContent};
use anyhow::Result;
use harnx_acp_server::permission::{A2UI_ACTION_OPTION_PREFIX, REJECT_OPTION_ID};
use harnx_core::session::SessionLogEntry;
use serde_json::json;

use super::harness::Fixture;

/// The ACP option ID of the surface's declared `deploy` action.
fn deploy_option() -> String {
    format!("{A2UI_ACTION_OPTION_PREFIX}deploy")
}

/// The text of a permission request's first content item, when it is a text block.
fn content_text(content: &ToolCallContent) -> Option<String> {
    match content {
        ToolCallContent::Content(content) => match &content.content {
            agent_client_protocol::v1::schema::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn deploy_surface() -> serde_json::Value {
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

async fn spawn_a2ui(id: &str) -> Result<Option<Fixture>> {
    let Some(f) = Fixture::spawn().await? else {
        return Ok(None);
    };
    f.request_payload(id, "a2ui", deploy_surface()).await?;
    f.ready(&[id]).await?;
    Ok(Some(f))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2ui_renders_surface_text_and_offers_declared_actions_plus_reject() -> Result<()> {
    let Some(mut f) = spawn_a2ui("form-1").await? else {
        return Ok(());
    };
    let request = f.permission().await?;
    let value = serde_json::to_value(&request)?;
    assert_eq!(value["_meta"]["harnx:interaction_id"], "form-1");
    // The shared text fallback carries the surface, not the summary.
    let message = match request
        .tool_call
        .fields
        .content
        .as_deref()
        .and_then(|content| content.first().and_then(content_text))
    {
        Some(text) => text,
        None => panic!("permission request carries no text content"),
    };
    assert!(message.contains("Pick a deployment target"));
    assert!(message.contains("Release name"));
    assert!(message.contains("Staging"));
    assert!(message.contains("cannot edit form inputs"));
    assert!(!message.contains("Summary fallback"));
    let options: Vec<_> = request
        .options
        .iter()
        .map(|option| (option.option_id.0.as_ref(), option.kind))
        .collect();
    assert_eq!(
        options,
        vec![
            (deploy_option().as_str(), PermissionOptionKind::AllowOnce),
            (REJECT_OPTION_ID, PermissionOptionKind::RejectOnce),
        ]
    );
    assert_eq!(request.options[0].name, "Deploy");
    f.quiet().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2ui_declared_action_answers_with_declared_defaults() -> Result<()> {
    let Some(mut f) = spawn_a2ui("form-2").await? else {
        return Ok(());
    };
    f.permission().await?;
    let response = f.answer("form-2", &deploy_option()).await?;
    assert_eq!(response.action, "deploy");
    // ACP's reply carries only an option ID: every input is answered with its
    // declared default through the shared response builder.
    assert_eq!(
        response.data,
        serde_json::Map::from_iter([
            ("choice".into(), json!(["staging"])),
            ("name".into(), json!("rel-1")),
        ])
    );
    assert!(response.note.is_none());
    f.quiet().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2ui_reject_dismisses_with_empty_data() -> Result<()> {
    let Some(mut f) = spawn_a2ui("form-3").await? else {
        return Ok(());
    };
    f.permission().await?;
    let response = f.answer("form-3", REJECT_OPTION_ID).await?;
    assert_eq!(response.action, "deny");
    assert!(response.data.is_empty());
    f.quiet().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2ui_option_that_was_not_offered_is_reported_and_never_journaled() -> Result<()> {
    let Some(mut f) = spawn_a2ui("form-4").await? else {
        return Ok(());
    };
    f.permission().await?;
    // "allow" was not offered for an a2ui interaction; only declared button
    // actions ("action:<event>") and Reject were. The reply is rejected before
    // durable submission.
    f.reply("allow")?;
    f.reported("not offered").await?;
    assert!(!f
        .log
        .load_events_latest_async()
        .await?
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::InteractionResponse { .. })));
    f.quiet().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2ui_conflicting_reply_is_reported_and_can_be_retried() -> Result<()> {
    let Some(mut f) = spawn_a2ui("form-5").await? else {
        return Ok(());
    };
    f.permission().await?;
    // The head is still pending after a failed acceptance; the observer
    // re-presents the same interaction and the corrected answer applies.
    f.force_outcomes
        .send(harnx_runtime::interactions::InteractionDecisionOutcome::Conflicting)?;
    f.reply(&deploy_option())?;
    f.reported("conflicting").await?;
    let retry = serde_json::to_value(f.permission().await?)?;
    assert_eq!(retry["_meta"]["harnx:interaction_id"], "form-5");
    let response = f.answer("form-5", &deploy_option()).await?;
    assert_eq!(response.action, "deploy");
    f.quiet().await;
    Ok(())
}
