//! Recover saved observations without registering servers or dispatching work.
use super::*;
use harnx_toolset_server::invocation_journal::InvocationJournal;

pub(in crate::nats_worker) async fn recover_completed_before_deadline(
    backend: &NatsSessionLogBackend,
    lease: &NatsSessionLease,
    config: &GlobalConfig,
    js: &jetstream::Context,
    replicas: usize,
) -> Result<()> {
    let entries = backend.load_events_latest_async().await?;
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
    let orphans = find_orphan_tool_calls(&effective);
    if orphans.is_empty() {
        return Ok(());
    }
    let journal = InvocationJournal::ensure(js, replicas).await?;
    for orphan in orphans {
        let mut results = Vec::new();
        let mut complete = true;
        for call in &orphan.calls {
            let record = match call.id.as_deref() {
                Some(id) => journal.find(backend.session_id(), orphan.seq, id).await?,
                None => None,
            };
            let saved = match record {
                Some(record) => saved_reply(&journal, &record, config).await?.map(|reply| {
                    super::super::wind_up::tool_output_from_reply(call, &record, reply)
                }),
                None => None,
            };
            match saved {
                Some(output) => results.push(output),
                None => complete = false,
            }
        }
        // Partial observations stay in the journal. A ToolResults batch would
        // hide unresolved calls from Cancel wind-up and skip their cleanup.
        if complete {
            let latest = backend.load_events_latest_async().await?;
            let tail = latest.last().map_or(0, |(seq, _)| *seq);
            backend
                .append_event_fenced_with_lease(
                    &SessionLogEntry::ToolResults {
                        results,
                        timestamp: orphan.timestamp,
                    },
                    lease,
                    tail,
                )
                .await?;
        }
    }
    Ok(())
}

/// The reply the journal saved for `record`'s call, or one recovered from the
/// child session a sub-agent call left behind, which is saved now.
async fn saved_reply(
    journal: &InvocationJournal,
    record: &harnx_toolset_server::invocation_journal::RecordedInvocation,
    config: &GlobalConfig,
) -> Result<Option<harnx_toolset::ToolReply>> {
    if let Some(reply) = journal.completed_reply(&record.request).await? {
        return Ok(Some(reply));
    }
    match recover_child_reply(record, config).await? {
        Some(reply) => Ok(Some(journal.complete(&record.request, reply).await?)),
        None => Ok(None),
    }
}

async fn recover_child_reply(
    record: &harnx_toolset_server::invocation_journal::RecordedInvocation,
    config: &GlobalConfig,
) -> Result<Option<harnx_toolset::ToolReply>> {
    if !matches!(
        record.request.tool.as_str(),
        "session_prompt" | "session_new"
    ) {
        return Ok(None);
    }
    let Some(checkpoint) = record.checkpoint.as_ref() else {
        return Ok(None);
    };
    let (Some(storage), Some(cluster), Some(session)) = (
        checkpoint["storage_key"].as_str(),
        checkpoint["cluster"].as_str(),
        checkpoint["session_id"].as_str(),
    ) else {
        return Ok(None);
    };
    let snapshot = config.read().clone();
    let js = snapshot.nats_jetstream(cluster).await?;
    let entries = crate::nats_session_log::NatsSessionLog::new(js, storage)
        .load_events_latest_async()
        .await?;
    let prompt = entries.iter().find_map(|(seq, entry)| match entry {
        SessionLogEntry::Message {
            id: Some(id), role, ..
        } if role.is_user() && id == &record.request.call_id => Some(*seq),
        _ => None,
    });
    let Some(prompt) = prompt else {
        return Ok(None);
    };
    if crate::nats_session::invocation_terminal_seq(&entries, prompt).is_none() {
        return Ok(None);
    }
    let (response, error) = crate::NatsSession::extract_turn_outcome(&entries, prompt);
    let was_cancelled =
        harnx_core::session_reconstruct::prompt_interrupted_at(&entries, prompt).is_some();
    Ok(Some(harnx_toolset::ToolReply {
        call_id: record.request.call_id.clone(),
        final_progress: None,
        result: Ok(
            serde_json::json!({"response": response, "error": error, "session_id": session, "was_cancelled": was_cancelled}),
        ),
    }))
}

#[cfg(test)]
#[path = "deadline_recovery_tests.rs"]
mod tests;
