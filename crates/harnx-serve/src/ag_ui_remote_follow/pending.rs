use super::*;

/// A worker can complete between lease polls. Hydrate its durable output even
/// when this attachment never saw a live lease or any advisory frames.
pub(super) async fn hydrate_completion(
    tx: &tokio::sync::mpsc::Sender<QueuedEvent>,
    event_stream: &SessionEventStream,
    session_id: &str,
    base: Option<&harnx_core::session::Session>,
) -> Result<()> {
    for event in completion_events(event_stream.history(), session_id, base)? {
        if tx
            .send(QueuedEvent {
                after_seq: event_stream.last_applied_seq(),
                event,
            })
            .await
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

/// Read the first terminal that answers this prompt, never a later run's error.
///
/// Deadline timeouts (`harnx:timeout {...}` in `requested_by`) map to `Error`
/// so the history-based hydration emits `RUN_ERROR`. Manual cancellation returns
/// `Finished`.
pub(crate) fn terminal_from_history(
    history: &[(u64, SessionLogEntry)],
    prompt_seq: u64,
) -> Option<RemoteFollowTerminal> {
    if prompt_seq == 0 {
        return None;
    }
    let Some((_, entry)) =
        harnx_core::session_reconstruct::prompt_terminal_entry(history, prompt_seq)
    else {
        // Retraction/rewind can settle input without a TurnEnd.
        return harnx_core::session_reconstruct::pending_prompt_seq(history)
            .is_none()
            .then_some(RemoteFollowTerminal::Finished);
    };
    Some(terminal_for_entry(entry))
}

fn terminal_for_entry(entry: &SessionLogEntry) -> RemoteFollowTerminal {
    match entry {
        SessionLogEntry::Error { message, .. } => RemoteFollowTerminal::Error(message.clone()),
        SessionLogEntry::Cancel { requested_by, .. } => requested_by
            .as_deref()
            .filter(|message| harnx_runtime::parse_timeout_terminal(message).is_some())
            .map(|message| RemoteFollowTerminal::Error(message.to_owned()))
            .unwrap_or(RemoteFollowTerminal::Finished),
        _ => RemoteFollowTerminal::Finished,
    }
}

pub(super) fn completion_events(
    history: &[(u64, SessionLogEntry)],
    session_id: &str,
    base: Option<&harnx_core::session::Session>,
) -> Result<Vec<Event>> {
    let Some(base) = base else {
        return Ok(Vec::new());
    };
    let session = harnx_runtime::nats_session_log::load_session_from_entries_with_metadata(
        history,
        session_id,
        base.clone(),
    )?;
    let snapshot = crate::transcript_snapshot::HistorySnapshot::from_entries(history, base, false)?;
    let usage = UsageContextSnapshot::from_session(&session);
    let attachments = crate::ag_ui::message_attachment_snapshot_events(&snapshot.messages, history);
    Ok(snapshot
        .events()
        .into_iter()
        .chain(attachments)
        .chain(crate::ag_ui::control_snapshot_events(history, Some(&usage)))
        .collect())
}
