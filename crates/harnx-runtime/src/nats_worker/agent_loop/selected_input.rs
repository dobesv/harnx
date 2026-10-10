use harnx_core::{message::Message, session::Session};

pub(super) fn restore_selected_user_messages(session: &mut Session, selected: &[Message]) {
    // A prompt arriving during summarization can precede Compress without being
    // in its snapshot or re-logged suffix. Restore only selected admissions, not
    // arbitrary archive rows, before tool rounds append their own context.
    let mut insert_at = session.messages.len();
    // Walk backwards so missing prompts precede later selected prompts that
    // survived the marker, rather than reversing concurrent submission order.
    for message in selected.iter().rev() {
        let existing = session
            .messages
            .iter()
            .position(|live| same_identity(live, message));
        if let Some(index) = existing {
            insert_at = index;
        } else if let Some(archived) = session
            .compressed_messages
            .iter()
            .rev()
            .find(|archived| same_identity(archived, message))
        {
            // Use effective replay content so a newer edit wins and a retraction
            // (absent from both live and archived history) isn't resurrected.
            session.messages.insert(insert_at, archived.clone());
        }
    }
    session.update_tokens();
}

fn same_identity(a: &Message, b: &Message) -> bool {
    if let Some(id) = b.id.as_ref() {
        a.id.as_ref() == Some(id)
    } else {
        b.log_seq.is_some() && a.log_seq == b.log_seq
    }
}

pub(crate) fn fold_new_user_messages_since(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    cursor: Option<u64>,
) -> (Vec<Message>, Option<u64>) {
    // Apply mutations first: retracted/edited entries must be filtered.
    let effective_entries = match harnx_core::session_reconstruct::apply_log_mutations_nats(entries)
    {
        Ok(entries) => entries,
        Err(err) => {
            log::warn!("failed to apply NATS log mutations while folding user messages: {err}");
            return (Vec::new(), cursor);
        }
    };

    let mut seen_ids = std::collections::HashSet::new();
    let mut messages = Vec::new();
    let mut latest_seq = cursor;
    for (seq, entry) in effective_entries {
        let harnx_core::session::SessionLogEntry::Message {
            id,
            role,
            content,
            timestamp,
            ..
        } = entry
        else {
            continue;
        };
        if !role.is_user() {
            continue;
        }
        // Register identities before cursor filtering: compaction copies have
        // newer physical sequences, but still represent the original admission.
        // ID-less legacy entries retain sequence-based behavior.
        let is_copy = id.as_ref().is_some_and(|id| !seen_ids.insert(id.clone()));
        if is_copy || cursor.is_some_and(|seen| seq <= seen) {
            continue;
        }
        let mut message = Message::new(role, content)
            .with_log_seq(usize::try_from(seq).expect("JetStream seq fits usize"))
            .with_log_timestamp(timestamp.unwrap_or_else(chrono::Utc::now));
        message.id = id;
        messages.push(message);
        latest_seq = Some(seq);
    }
    (messages, latest_seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{MessageContent, MessageRole};

    fn user(id: Option<&str>, seq: usize) -> Message {
        let mut message = Message::new(MessageRole::User, MessageContent::Text("same text".into()))
            .with_log_seq(seq);
        message.id = id.map(str::to_owned);
        message
    }

    #[test]
    fn restore_selected_prompts_preserves_order_and_effective_edits_without_retractions() {
        let mut edited = user(Some("missing"), 18);
        edited.content = MessageContent::Text("edited input".into());
        let mut session = Session {
            messages: vec![user(Some("later"), 9)],
            compressed_messages: vec![edited],
            ..Default::default()
        };
        let selected = vec![
            user(Some("missing"), 5),
            user(Some("retracted"), 6),
            user(Some("later"), 9),
        ];
        restore_selected_user_messages(&mut session, &selected);
        restore_selected_user_messages(&mut session, &selected);
        assert_eq!(
            session
                .messages
                .iter()
                .map(|m| (m.content.to_text(), m.log_seq))
                .collect::<Vec<_>>(),
            vec![
                ("edited input".into(), Some(18)),
                ("same text".into(), Some(9))
            ]
        );
    }

    #[test]
    fn restore_selected_prompts_deduplicates_ids_not_text_or_relogged_sequence() {
        let mut session = Session {
            messages: vec![user(Some("consumed"), 10)],
            compressed_messages: vec![
                user(Some("archived"), 1),
                user(Some("new"), 5),
                user(None, 6),
            ],
            ..Default::default()
        };
        let selected = vec![
            user(Some("consumed"), 3),
            user(Some("new"), 5),
            user(None, 6),
        ];
        restore_selected_user_messages(&mut session, &selected);
        restore_selected_user_messages(&mut session, &selected);
        assert_eq!(
            session
                .messages
                .iter()
                .map(|m| (m.id.as_deref(), m.log_seq))
                .collect::<Vec<_>>(),
            vec![
                (Some("consumed"), Some(10)),
                (Some("new"), Some(5)),
                (None, Some(6))
            ]
        );
    }
}
