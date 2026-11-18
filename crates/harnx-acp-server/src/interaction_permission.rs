//! Durable middleware interactions share ACP's single-turn permission UI with legacy HITL.

mod legacy;
mod observe;
pub(crate) use legacy::LegacyProbe;

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, TextContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::interactions::{InteractionDecisionOutcome, PendingInteraction};
use harnx_runtime::NatsSession;
use tokio::sync::{watch, Mutex, RwLock};
use tokio_util::task::AbortOnDropHandle;

use crate::permission::{permission_options, ALLOW_OPTION_ID, REJECT_OPTION_ID};
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
    let supported = pending.request.kind == "confirm";
    let message = pending
        .request
        .payload
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| pending.summary());
    let message = if supported {
        message.to_string()
    } else {
        format!(
            "{}\n\nInteraction kind '{}' is not supported by this client. Reject to continue.",
            message, pending.request.kind
        )
    };
    let fields = ToolCallUpdateFields::new()
        .title(message.clone())
        .status(ToolCallStatus::Pending)
        .content(vec![ContentBlock::Text(TextContent::new(message)).into()]);
    let options = if supported {
        permission_options()
    } else {
        vec![PermissionOption::new(
            REJECT_OPTION_ID,
            "Reject",
            PermissionOptionKind::RejectOnce,
        )]
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

fn decision_response(
    pending: &PendingInteraction,
    outcome: RequestPermissionOutcome,
) -> Result<InteractionResponse, &'static str> {
    let action = match outcome {
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            ALLOW_OPTION_ID if pending.request.kind == "confirm" => "approve",
            REJECT_OPTION_ID => "deny",
            _ => return Err("client selected an option that was not offered"),
        },
        RequestPermissionOutcome::Cancelled => "deny",
        _ => return Err("client returned an unsupported permission outcome"),
    };
    // ACP v1 SelectedPermissionOutcome carries only option_id, not a free-form note.
    Ok(InteractionResponse {
        action: action.into(),
        data: serde_json::Map::new(),
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
