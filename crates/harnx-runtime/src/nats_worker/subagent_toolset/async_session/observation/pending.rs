//! Keep actionable child identity after expendable observation text is removed.
use harnx_core::{event::SubAgentProgress, pending_interaction::PendingInteractionSummary};
use serde_json::{json, Map, Value};

/// The fields [`attach_pending_progress`] adds to a blocked result, computed
/// once so a caller can also reserve their exact serialized size before the
/// page is fitted (plan #2970 T3).
pub(in crate::nats_worker::subagent_toolset) struct PendingProgressFields {
    pub pending_interaction: Value,
    pub sub_agent_progress: Value,
    pub sub_agent: Value,
}

/// Mandatory identity only: the summary with its expendable message gone is
/// the floor every refit keeps room for.
pub(in crate::nats_worker::subagent_toolset) fn pending_identity(
    pending: &PendingInteractionSummary,
) -> Value {
    let mut identity = json!(pending);
    identity["message"] = json!("");
    identity
}

/// The pending-interaction fields a blocked result carries, for the caller
/// that reserves their exact serialized size before fitting the page.
pub(in crate::nats_worker::subagent_toolset) fn pending_progress_fields(
    pending: &PendingInteractionSummary,
    prompt_id: &str,
    live: Option<SubAgentProgress>,
) -> PendingProgressFields {
    let child = super::progress::ChildIdentity {
        agent: pending.agent.clone(),
        session_id: pending.session_id.clone(),
        prompt_id: prompt_id.to_string(),
    };
    let mut progress = super::progress::terminal_progress_value(
        &child,
        super::SessionPromptStatus::Blocked,
        None,
        live,
    );
    progress.pending_interaction = Some(pending.clone());
    PendingProgressFields {
        pending_interaction: json!(pending),
        sub_agent_progress: json!(progress),
        sub_agent: json!({"agent": child.agent, "session_id": child.session_id}),
    }
}

/// Serialized bytes inserting these fields into a non-empty result object
/// costs: the probe without its braces, one separator per inserted pair
/// instead of the probe's internal ones.
pub(in crate::nats_worker::subagent_toolset) fn progress_fields_reserve(
    fields: &PendingProgressFields,
) -> usize {
    attached_bytes(&[
        ("pending_interaction", &fields.pending_interaction),
        ("sub_agent_progress", &fields.sub_agent_progress),
        ("sub_agent", &fields.sub_agent),
    ])
}

/// The reserve for the mandatory pending-interaction identity: the summary
/// with an empty message, so trimming the real message down to that floor
/// still fits the reserved space.
pub(in crate::nats_worker::subagent_toolset) fn pending_identity_reserve(
    pending: &PendingInteractionSummary,
) -> usize {
    attached_bytes(&[("pending_interaction", &pending_identity(pending))])
}

fn attached_bytes(pairs: &[(&str, &Value)]) -> usize {
    let mut probe = Map::new();
    for (name, value) in pairs {
        probe.insert((*name).to_string(), (*value).clone());
    }
    serde_json::to_vec(&Value::Object(probe))
        // A `serde_json::Value` object always serializes; the impossible
        // failure must over-reserve (a page that fits can never be cut short),
        // never under-reserve.
        .map_or(usize::MAX, |bytes| bytes.len())
        .saturating_sub(1)
}

pub(in crate::nats_worker::subagent_toolset) fn attach_pending_progress(
    value: &mut Value,
    pending: PendingInteractionSummary,
    live: Option<SubAgentProgress>,
    max_bytes: usize,
) {
    let prompt_id = value["prompt_id"].as_str().unwrap_or_default().to_owned();
    let fields = pending_progress_fields(&pending, &prompt_id, live);
    value["pending_interaction"] = fields.pending_interaction;
    value["sub_agent_progress"] = fields.sub_agent_progress;
    value["sub_agent"] = fields.sub_agent;
    if !oversized(value, max_bytes) {
        return;
    }
    for field in ["activity", "sub_agent_progress", "sub_agent"] {
        if oversized(value, max_bytes) {
            remove_field(value, field);
        }
    }
    fit_summary_message(value, max_bytes);
}

/// Attach only the mandatory pending-interaction identity, trimming the
/// message (never an identifier) until the result fits. Used by refits that
/// dropped the optional progress fields to keep the page moving.
pub(in crate::nats_worker::subagent_toolset) fn attach_pending_identity(
    value: &mut Value,
    pending: PendingInteractionSummary,
    max_bytes: usize,
) {
    value["pending_interaction"] = json!(pending);
    fit_summary_message(value, max_bytes);
}

fn remove_field(value: &mut Value, field: &str) {
    if let Some(object) = value.as_object_mut() {
        object.remove(field);
    }
}

fn fit_summary_message(value: &mut Value, max_bytes: usize) {
    // Never shorten identity: a truncated identifier could address another request.
    while oversized(value, max_bytes) {
        let message = value["pending_interaction"]["message"]
            .as_str()
            .unwrap_or_default();
        if message.is_empty() {
            break;
        }
        let end = message.char_indices().next_back().map_or(0, |(i, _)| i);
        value["pending_interaction"]["message"] = json!(&message[..end]);
    }
}

fn oversized(value: &Value, max_bytes: usize) -> bool {
    value.to_string().len() > max_bytes
}
