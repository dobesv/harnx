//! Evaluate retained readiness proof without depending on frontend model types.
use super::*;

pub(crate) async fn unresolved(metadata: &SessionMetadataStore, storage: &str) -> Result<bool> {
    let key = format!("sessions/{storage}/a2a/context");
    let Some(entry) = metadata.leader_entry(&key).await? else {
        return Ok(false);
    };
    if entry.operation != Operation::Put {
        return Ok(false);
    }
    entry_unresolved(&entry)
}

pub(super) fn entry_unresolved(entry: &async_nats::jetstream::kv::Entry) -> Result<bool> {
    if entry.operation != Operation::Put {
        return Ok(false);
    }
    let doc: Value = serde_json::from_slice(&entry.value)?;
    let active = doc
        .pointer("/state/active")
        .ok_or_else(|| anyhow::anyhow!("invalid A2A context state"))?;
    Ok(!active.is_null() && !retired(active))
}

fn retired(active: &Value) -> bool {
    matches!(
        active
            .pointer("/snapshot/task/status/state")
            .and_then(Value::as_str),
        Some(
            "TASK_STATE_COMPLETED"
                | "TASK_STATE_FAILED"
                | "TASK_STATE_CANCELED"
                | "TASK_STATE_REJECTED"
        )
    ) && [
        "/stop_confirmed",
        "/projections/archive",
        "/projections/message_mapping",
        "/projections/final_event",
    ]
    .iter()
    .all(|p| active.pointer(p).and_then(Value::as_bool) == Some(true))
        && active
            .pointer("/publication/pending")
            .is_some_and(Value::is_null)
}

pub(super) async fn ensure_first_settled(
    metadata: &SessionMetadataStore,
    registry: Option<&Value>,
    storage: &str,
    context: Option<&async_nats::jetstream::kv::Entry>,
) -> Result<()> {
    let Some(identity) = registry.and_then(|doc| doc.pointer("/first/identity")) else {
        return Ok(());
    };
    let key = format!(
        "a2a/first-messages/{}",
        sha256(&serde_json::to_string(identity)?)
    );
    let Some(bytes) = metadata.leader_value(&key).await? else {
        return Ok(());
    };
    let first: Value = serde_json::from_slice(&bytes)?;
    if first
        .pointer("/allocation/storage_key")
        .and_then(Value::as_str)
        != Some(storage)
    {
        return Ok(());
    }
    if let Some(entry) = context {
        let doc: Value = serde_json::from_slice(&entry.value)?;
        if doc
            .pointer("/state/active")
            .is_some_and(|active| !active.is_null())
        {
            return Ok(());
        }
    }
    let task = first
        .pointer("/allocation/task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("invalid A2A reservation task"))?;
    let (_, uuid) = task
        .rsplit_once('.')
        .ok_or_else(|| anyhow::anyhow!("invalid A2A task identity"))?;
    ensure!(
        metadata
            .leader_value(&format!("sessions/{storage}/a2a/archive/{uuid}"))
            .await?
            .is_some(),
        "A2A first admission is unallocated or unsettled; deletion refused"
    );
    Ok(())
}
