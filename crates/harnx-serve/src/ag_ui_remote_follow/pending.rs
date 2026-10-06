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
pub(crate) fn terminal_from_history(
    history: &[(u64, SessionLogEntry)],
    prompt_seq: u64,
) -> Option<RemoteFollowTerminal> {
    if prompt_seq == 0 {
        return None;
    }
    if let Some((_, entry)) =
        harnx_core::session_reconstruct::prompt_terminal_entry(history, prompt_seq)
    {
        return Some(match entry {
            SessionLogEntry::Error { message, .. } => RemoteFollowTerminal::Error(message.clone()),
            _ => RemoteFollowTerminal::Finished,
        });
    }
    // Retraction/rewind can settle input without a TurnEnd.
    harnx_core::session_reconstruct::pending_prompt_seq(history)
        .is_none()
        .then_some(RemoteFollowTerminal::Finished)
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
    let snapshot = crate::ag_ui::history_messages_for_snapshot(&session.messages);
    let usage = UsageContextSnapshot::from_session(&session);
    Ok(std::iter::once(snapshot_event(snapshot.clone()))
        .chain(crate::ag_ui::message_attachment_snapshot_events(
            &snapshot, history,
        ))
        .chain(crate::ag_ui::control_snapshot_events(history, Some(&usage)))
        .collect())
}
