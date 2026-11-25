//! Durable middleware interactions share ACP's single-turn permission UI with legacy HITL.

mod legacy;
mod observe;
pub(crate) use legacy::LegacyProbe;

use std::sync::Arc;

use agent_client_protocol::v1::schema::{
    ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, TextContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use harnx_core::interaction::{
    response::{build_data, button_actions},
    text, InteractionResponse,
};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::interactions::{InteractionDecisionOutcome, PendingInteraction};
use harnx_runtime::NatsSession;
use tokio::sync::{watch, Mutex, RwLock};
use tokio_util::task::AbortOnDropHandle;

use crate::permission::{
    permission_options, A2UI_ACTION_OPTION_PREFIX, ALLOW_OPTION_ID, REJECT_OPTION_ID,
};
use crate::AcpConnection;

pub(crate) type Connections = Arc<RwLock<Option<AcpConnection>>>;

/// Owned by SessionContext, never by the observer task (no session/task reference cycle).
pub(crate) struct SessionPermissions {
    pub(crate) gate: Arc<Mutex<()>>,
    cancel: watch::Sender<u64>,
    observer: parking_lot::Mutex<Option<AbortOnDropHandle<()>>>,
    source: parking_lot::Mutex<Option<LegacyProbe>>,
}

impl Default for SessionPermissions {
    fn default() -> Self {
        Self {
            gate: Arc::new(Mutex::new(())),
            cancel: watch::channel(0).0,
            observer: parking_lot::Mutex::new(None),
            source: parking_lot::Mutex::new(None),
        }
    }
}

impl SessionPermissions {
    pub(crate) fn observe(
        &self,
        session: NatsSession,
        config: GlobalConfig,
        cluster: String,
        connections: Connections,
    ) {
        *self.source.lock() = Some(LegacyProbe {
            config: config.clone(),
            cluster: cluster.clone(),
            storage_key: session.storage_key().to_string(),
        });
        let observer = observe::Observer {
            session,
            config,
            cluster,
            connections,
            gate: Arc::clone(&self.gate),
            cancel: self.cancel.subscribe(),
        };
        *self.observer.lock() = Some(AbortOnDropHandle::new(tokio::spawn(observer.run())));
    }

    pub(crate) fn legacy_probe(&self) -> anyhow::Result<LegacyProbe> {
        self.source
            .lock()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("session permission observer is not initialized"))
    }

    pub(crate) fn cancel_request(&self) {
        self.cancel
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

fn permission_request(session_id: &str, pending: &PendingInteraction) -> RequestPermissionRequest {
    let (title, message) = match pending.request.kind.as_str() {
        "confirm" => {
            let message = pending
                .request
                .payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| pending.summary());
            let message = message.to_string();
            (message.clone(), message)
        }
        // ACP has no form widgets: render the surface with the shared text
        // fallback so the user can read every field, option, and action.
        "a2ui" => (
            pending.summary().to_string(),
            a2ui_message(&pending.request.payload),
        ),
        kind => {
            let message = format!(
                "{}\n\nInteraction kind '{kind}' is not supported by this client. Reject to continue.",
                pending.summary()
            );
            (message.clone(), message)
        }
    };
    let fields = ToolCallUpdateFields::new()
        .title(title)
        .status(ToolCallStatus::Pending)
        .content(vec![ContentBlock::Text(TextContent::new(message)).into()]);
    let options = match pending.request.kind.as_str() {
        "confirm" => permission_options(),
        // ACP's permission reply carries only an option ID, so offer one
        // option per declared button action plus Reject (deny).
        "a2ui" => a2ui_options(&pending.request.payload),
        _ => vec![PermissionOption::new(
            REJECT_OPTION_ID,
            "Reject",
            PermissionOptionKind::RejectOnce,
        )],
    };
    // Bind the RPC's response to these coordinates, not to a tool ID or the next pending item.
    let mut meta = serde_json::Map::new();
    meta.insert(
        "harnx:interaction_id".into(),
        pending.interaction_id.clone().into(),
    );
    meta.insert("harnx:request_seq".into(), pending.request_seq.into());
    RequestPermissionRequest::new(
        session_id.to_string(),
        ToolCallUpdate::new(pending.tool_call_id.clone(), fields),
        options,
    )
    .meta(meta)
}

/// The a2ui text the user sees: the shared plain-text rendering plus a hint
/// that this client cannot collect form inputs.
fn a2ui_message(payload: &serde_json::Value) -> String {
    let mut message = text::render(payload);
    message.push_str(
        "\n\nThis client cannot edit form inputs. Each input is answered with its \
         declared default; to supply values use the Web UI, TUI, or \
         `harnx interact answer`.",
    );
    message
}

/// One AllowOnce option per declared button action (labelled by its button
/// text when it has one), plus the Reject dismissal option.
fn a2ui_options(payload: &serde_json::Value) -> Vec<PermissionOption> {
    let mut options = button_actions(payload)
        .into_iter()
        .map(|action| {
            let label = action.label.clone().unwrap_or_else(|| action.name.clone());
            PermissionOption::new(
                format!("{A2UI_ACTION_OPTION_PREFIX}{}", action.name),
                label,
                PermissionOptionKind::AllowOnce,
            )
        })
        .collect::<Vec<_>>();
    options.push(PermissionOption::new(
        REJECT_OPTION_ID,
        "Reject",
        PermissionOptionKind::RejectOnce,
    ));
    options
}

fn decision_response(
    pending: &PendingInteraction,
    outcome: RequestPermissionOutcome,
) -> Result<InteractionResponse, String> {
    let kind = pending.request.kind.as_str();
    let action = match outcome {
        RequestPermissionOutcome::Selected(selected) => {
            let option_id = selected.option_id.0.as_ref();
            if option_id == REJECT_OPTION_ID {
                "deny".to_string()
            } else if option_id == ALLOW_OPTION_ID && kind == "confirm" {
                "approve".to_string()
            } else if kind == "a2ui" {
                option_id
                    .strip_prefix(A2UI_ACTION_OPTION_PREFIX)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        format!("client selected an option that was not offered: '{option_id}'")
                    })?
            } else {
                return Err(format!(
                    "client selected an option that was not offered: '{option_id}'"
                ));
            }
        }
        RequestPermissionOutcome::Cancelled => "deny".to_string(),
        _ => return Err("client returned an unsupported permission outcome".into()),
    };
    // ACP's permission reply carries only an option ID, so an a2ui answer
    // supplies no input values: the shared builder fills every bound input
    // with its declared default and resolves the action's declared context.
    let data = if kind == "a2ui" {
        build_data(&pending.request.payload, &action, &serde_json::Map::new())
            .map_err(|error| format!("cannot build a2ui response data: {error}"))?
    } else {
        serde_json::Map::new()
    };
    // ACP v1 SelectedPermissionOutcome carries only option_id, not a free-form note.
    Ok(InteractionResponse {
        action,
        data,
        note: None,
    })
}

fn decision_error(outcome: InteractionDecisionOutcome) -> Option<&'static str> {
    match outcome {
        InteractionDecisionOutcome::Applied | InteractionDecisionOutcome::AlreadyApplied => None,
        InteractionDecisionOutcome::Conflicting => Some("conflicting"),
        InteractionDecisionOutcome::WrongSequence => Some("wrong_sequence"),
        InteractionDecisionOutcome::Stale => Some("stale"),
        InteractionDecisionOutcome::AfterComplete => Some("after_complete"),
        InteractionDecisionOutcome::InvalidResponse => Some("invalid_response"),
        InteractionDecisionOutcome::LeaseLost => Some("lease_lost"),
    }
}

pub(super) fn report(connection: &AcpConnection, session_id: &str, message: String) {
    use harnx_core::event::{AgentEvent, NoticeEvent};
    if let Some(update) = crate::event_map::agent_event_to_session_update(AgentEvent::Notice(
        NoticeEvent::Warning(format!("\nMiddleware interaction: {message}\n")),
    )) {
        crate::forward_update(Some(connection), session_id, update);
    }
}

async fn attach_stream(
    config: &GlobalConfig,
    cluster: &str,
    storage_key: &str,
) -> anyhow::Result<harnx_runtime::nats_event_sink::SessionEventStream> {
    let snapshot = config.read().clone();
    let client = snapshot.nats_client(cluster).await?;
    let jetstream = snapshot.nats_jetstream(cluster).await?;
    harnx_runtime::nats_event_sink::SessionEventStream::attach(jetstream, client, storage_key).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{
        PermissionOptionId, SelectedPermissionOutcome, ToolCallContent,
    };
    use harnx_core::interaction::InteractionRequest;
    use serde_json::json;

    /// The text of a permission request's first content item, when it is a text
    /// block.
    fn content_text(content: &ToolCallContent) -> Option<String> {
        match content {
            ToolCallContent::Content(content) => match &content.content {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    // qual:allow(dry, duplicate) reason: "test-only PendingInteraction builder; the harnx CLI has an identical one, and the crates share no test utilities"
    fn pending(kind: &str, payload: serde_json::Value) -> PendingInteraction {
        PendingInteraction {
            request_seq: 7,
            tool_round_seq: 1,
            tool_call_id: "call-1".into(),
            interaction_id: "int-1".into(),
            request: InteractionRequest {
                interaction_id: "int-1".into(),
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

    fn selected(option_id: &str) -> RequestPermissionOutcome {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(PermissionOptionId::new(
            option_id.to_string(),
        )))
    }

    fn a2ui_payload() -> serde_json::Value {
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

    #[test]
    fn a2ui_permission_renders_surface_text_and_offers_actions() {
        let request = permission_request("acp-session", &pending("a2ui", a2ui_payload()));
        assert_eq!(
            request.tool_call.fields.title.as_deref(),
            Some("Pick a deployment target")
        );
        // The shared text fallback shows the headline, the inputs with their
        // defaults, and the actions footer.
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
        assert!(message.contains("Release name"));
        assert!(message.contains("Staging"));
        assert!(message.contains("deploy"));
        assert!(message.contains("cannot edit form inputs"));
        let options: Vec<_> = request
            .options
            .iter()
            .map(|option| (option.option_id.0.as_ref(), option.kind))
            .collect();
        assert_eq!(
            options,
            vec![
                ("action:deploy", PermissionOptionKind::AllowOnce),
                (REJECT_OPTION_ID, PermissionOptionKind::RejectOnce),
            ]
        );
        assert_eq!(request.options[0].name, "Deploy");
        let value = serde_json::to_value(&request).expect("serializable");
        assert_eq!(value["_meta"]["harnx:interaction_id"], "int-1");
        assert_eq!(value["_meta"]["harnx:request_seq"], 7);
    }

    #[test]
    fn a2ui_action_selection_answers_with_declared_defaults() {
        let response =
            decision_response(&pending("a2ui", a2ui_payload()), selected("action:deploy"))
                .expect("declared action");
        assert_eq!(response.action, "deploy");
        // ACP collects no input values, so the shared builder supplies the
        // declared dataModel defaults.
        assert_eq!(
            response.data,
            serde_json::Map::from_iter([
                ("choice".into(), json!(["staging"])),
                ("name".into(), json!("rel-1")),
            ])
        );
        assert!(response.note.is_none());
    }

    #[test]
    fn a2ui_reject_and_cancel_dismiss() {
        for outcome in [
            selected(REJECT_OPTION_ID),
            RequestPermissionOutcome::Cancelled,
        ] {
            let response = decision_response(&pending("a2ui", a2ui_payload()), outcome)
                .expect("dismissal is always accepted");
            assert_eq!(response.action, "deny");
            assert!(response.data.is_empty());
        }
    }

    #[test]
    fn a2ui_unknown_option_is_rejected() {
        let error = decision_response(&pending("a2ui", a2ui_payload()), selected("allow"))
            .expect_err("allow was not offered for a2ui");
        assert!(error.contains("not offered"));
        assert!(decision_response(
            &pending("a2ui", a2ui_payload()),
            selected("action:rollback")
        )
        .is_err());
    }

    #[test]
    fn confirm_outcomes_are_unchanged() {
        let confirm = pending("confirm", json!({"message": "Deploy to production?"}));
        assert_eq!(
            decision_response(&confirm, selected(ALLOW_OPTION_ID))
                .expect("allow")
                .action,
            "approve"
        );
        assert_eq!(
            decision_response(&confirm, selected(REJECT_OPTION_ID))
                .expect("reject")
                .action,
            "deny"
        );
        assert!(decision_response(&confirm, selected("action:deploy")).is_err());
    }
}
