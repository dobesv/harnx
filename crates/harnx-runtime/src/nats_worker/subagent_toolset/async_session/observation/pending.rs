//! Keep actionable child identity after expendable observation text is removed.
use harnx_core::{event::SubAgentProgress, pending_interaction::PendingInteractionSummary};
use serde_json::{json, Value};

pub(in crate::nats_worker::subagent_toolset) fn attach_pending_progress(
    value: &mut Value,
    pending: PendingInteractionSummary,
    live: Option<SubAgentProgress>,
    max_bytes: usize,
) {
    let child = super::progress::ChildIdentity {
        agent: pending.agent.clone(),
        session_id: pending.session_id.clone(),
        prompt_id: value["prompt_id"].as_str().unwrap_or_default().to_owned(),
    };
    let mut progress = super::progress::terminal_progress_value(
        &child,
        super::SessionPromptStatus::Blocked,
        None,
        live,
    );
    progress.pending_interaction = Some(pending.clone());
    value["pending_interaction"] = json!(pending);
    value["sub_agent_progress"] = json!(progress);
    value["sub_agent"] = json!({"agent": child.agent, "session_id": child.session_id});
    if !oversized(value, max_bytes) {
        return;
    }
    drop_page_events(value);
    for field in ["sub_agent_progress", "sub_agent"] {
        if oversized(value, max_bytes) {
            remove_field(value, field);
        }
    }
    fit_summary_message(value, max_bytes);
}

fn remove_field(value: &mut Value, field: &str) {
    if let Some(object) = value.as_object_mut() {
        object.remove(field);
    }
}

fn drop_page_events(value: &mut Value) {
    // Preserve a restart cursor rather than implying skipped events were delivered.
    let page = if value.get("output").is_some() {
        &mut value["output"]
    } else {
        value
    };
    let next = page["events"]
        .as_array()
        .and_then(|events| events.first())
        .and_then(|e| e.get("index"))
        .cloned()
        .unwrap_or_else(|| page["next_index"].clone());
    for field in ["omitted_ranges", "continuation"] {
        remove_field(page, field);
    }
    page["events"] = json!([]);
    page["truncated"] = json!(true);
    page["next_index"] = next;
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
