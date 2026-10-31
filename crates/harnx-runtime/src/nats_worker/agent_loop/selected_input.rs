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
            .position(|live| live.same_identity(message));
        if let Some(index) = existing {
            insert_at = index;
        } else if let Some(archived) = session
            .compressed_messages
            .iter()
            .rev()
            .find(|archived| archived.same_identity(message))
        {
            // Use effective replay content so a newer edit wins and a retraction
            // (absent from both live and archived history) isn't resurrected.
            session.messages.insert(insert_at, archived.clone());
        }
    }
    session.update_tokens();
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
        if !super::is_executable_user_message(id.as_deref(), &role, &content) {
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
        latest_seq = Some(advance_cursor(latest_seq, seq));
    }
    (messages, latest_seq)
}

fn advance_cursor(cursor: Option<u64>, seq: u64) -> u64 {
    // Edits resequence rows without changing conversation order. Track the max,
    // not the last selected sequence, or an earlier-positioned row repeats.
    cursor.map_or(seq, |seen| seen.max(seq))
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

#[cfg(test)]
mod timing_fold_tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::SessionLogEntry;
    use harnx_core::timing_identity::{TimingReceipt, TIMING_HEADER};

    /// Create a valid timing notice entry with proper JSON receipt
    fn timing_notice(seq: u64) -> (u64, SessionLogEntry) {
        let origin = Utc.with_ymd_and_hms(2024, 1, 15, 12, 0, 0).unwrap();
        let receipt = TimingReceipt::initial(origin, "inv-123".to_string());
        (
            seq,
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(format!(
                    "{}{} | Elapsed: 0s",
                    TIMING_HEADER,
                    receipt.to_json()
                )),
                timestamp: None,
                fence_token: None,
            },
        )
    }

    fn user_prompt(seq: u64, text: &str) -> (u64, SessionLogEntry) {
        (
            seq,
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(text.to_string()),
                timestamp: None,
                fence_token: None,
            },
        )
    }

    /// Create a user prompt with reserved header prefix but invalid JSON
    fn user_prompt_with_reserved_prefix(seq: u64, text: &str) -> (u64, SessionLogEntry) {
        user_prompt(seq, &format!("{} {}", TIMING_HEADER, text))
    }

    #[test]
    fn fold_new_user_messages_since_skips_timing_notices() {
        let entries: Vec<(u64, SessionLogEntry)> = vec![
            user_prompt(1, "prompt one"),
            timing_notice(2),
            user_prompt(3, "prompt two"),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);
        // Only real prompts should be returned, timing notice at seq 2 skipped
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content.to_text(), "prompt one");
        assert_eq!(messages[1].content.to_text(), "prompt two");
        assert_eq!(latest_seq, Some(3));
    }

    #[test]
    fn fold_timing_notice_with_concurrent_user_preserves_real_user() {
        // Timing notice interleaved with concurrent real user messages
        let entries: Vec<(u64, SessionLogEntry)> = vec![
            user_prompt(1, "initial prompt"),
            timing_notice(2),
            timing_notice(3),
            user_prompt(4, "second prompt"),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);
        // All timing notices skipped
        assert_eq!(messages.len(), 2);
        assert_eq!(latest_seq, Some(4));
    }

    #[test]
    fn fold_cursor_tracks_real_users_not_timing_notices() {
        // Cursor should track real user prompts, not timing notices
        let entries: Vec<(u64, SessionLogEntry)> = vec![
            user_prompt(1, "first"),
            timing_notice(2),
            user_prompt(3, "second"),
        ];

        // After first user at seq 1, next fold should skip timing and return second
        let (messages1, cursor1) = fold_new_user_messages_since(&entries, None);
        assert_eq!(messages1.len(), 2);
        assert_eq!(cursor1, Some(3));

        // After second user at seq 3, should return empty
        let (messages2, cursor2) = fold_new_user_messages_since(&entries, Some(3));
        assert!(messages2.is_empty());
        assert_eq!(cursor2, Some(3));
    }

    #[test]
    fn fold_includes_user_prompts_with_reserved_prefix_but_invalid_receipt() {
        // Regression: Real user prompts starting with reserved header but without valid JSON
        // must NOT be skipped - they should be treated as normal user prompts
        let entries: Vec<(u64, SessionLogEntry)> = vec![
            user_prompt(1, "first"),
            user_prompt_with_reserved_prefix(2, "second - invalid timing format"),
            user_prompt(3, "third"),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);
        // All 3 prompts should be returned (invalid timing format is NOT skipped)
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content.to_text(), "first");
        assert!(messages[1]
            .content
            .to_text()
            .contains("invalid timing format"));
        assert_eq!(messages[2].content.to_text(), "third");
        assert_eq!(latest_seq, Some(3));
    }
}

#[cfg(test)]
mod runtime_note_and_cursor_tests {
    use super::*;
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::SessionLogEntry;

    fn message_row(id: Option<&str>, text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: id.map(str::to_owned),
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
            timestamp: None,
            fence_token: None,
        }
    }

    #[test]
    fn fold_skips_runtime_notes_and_keeps_lookalikes_executable() {
        let note_id = harnx_core::runtime_note::new_runtime_note_message_id();
        let entries = vec![
            (1, message_row(Some("real-1"), "first prompt")),
            // Compaction re-log of a replayed sub-agent note: structured id.
            (
                2,
                message_row(
                    Some(&note_id),
                    &format!(
                        "{}Started sub-agent 'child' in session child-session.",
                        harnx_core::runtime_note::RUNTIME_NOTE_PREFIX
                    ),
                ),
            ),
            // An ID-less lookalike is NOT classified by text: it stays
            // executable input (fail-closed at admission verification).
            (
                3,
                message_row(
                    None,
                    &format!(
                        "{}The user interrupted this turn.",
                        harnx_core::runtime_note::RUNTIME_NOTE_PREFIX
                    ),
                ),
            ),
            (4, message_row(Some("real-2"), "second prompt")),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);
        let texts: Vec<String> = messages.iter().map(|m| m.content.to_text()).collect();
        let interrupted = format!(
            "{}The user interrupted this turn.",
            harnx_core::runtime_note::RUNTIME_NOTE_PREFIX
        );
        assert_eq!(
            texts,
            vec![
                "first prompt".to_string(),
                interrupted,
                "second prompt".to_string()
            ]
        );
        assert_eq!(latest_seq, Some(4));
    }

    #[test]
    fn authored_prompt_with_note_prefix_keeps_its_admission_identity() {
        // A user prompt that merely starts with the runtime-note prefix is
        // admitted with an id, so it must stay executable input.
        let entries = vec![(
            1,
            message_row(
                Some("admitted-lookalike"),
                &format!(
                    "{}lookalike prompt",
                    harnx_core::runtime_note::RUNTIME_NOTE_PREFIX
                ),
            ),
        )];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id.as_deref(), Some("admitted-lookalike"));
        assert_eq!(latest_seq, Some(1));
    }

    #[test]
    fn fold_cursor_is_max_selected_seq_when_edits_resequence_rows() {
        // An EditEntries replacement inherits the edit's physical sequence but
        // stays at the original row's conversation position, so effective
        // sequences are not monotonic across the selected set. The cursor must
        // be the maximum selected sequence; the last selected row's sequence
        // would leave the earlier-positioned row re-selected every round.
        let edited = serde_yaml::to_string(&message_row(Some("edited"), "edited prompt")).unwrap();
        let entries = vec![
            (1, message_row(Some("edited"), "original prompt")),
            (2, message_row(Some("later"), "later prompt")),
            (
                3,
                SessionLogEntry::EditEntries {
                    from: 1,
                    to: 1,
                    replacements: vec![edited],
                },
            ),
        ];

        let (messages, cursor) = fold_new_user_messages_since(&entries, None);
        let texts: Vec<String> = messages.iter().map(|m| m.content.to_text()).collect();
        assert_eq!(texts, vec!["edited prompt", "later prompt"]);
        assert_eq!(cursor, Some(3), "cursor must be the max selected seq");

        // The next round must not re-select the edited row at its higher seq.
        let (again, _) = fold_new_user_messages_since(&entries, cursor);
        assert!(again.is_empty());
    }
}
