use super::fold_new_user_messages_since;
use super::tests::user_entry;
use chrono::{TimeZone, Utc};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;

fn message(id: Option<&str>, role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: id.map(str::to_owned),
        role,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn user(id: &str, text: &str) -> SessionLogEntry {
    message(Some(id), MessageRole::User, text)
}

fn assistant(text: &str) -> SessionLogEntry {
    message(None, MessageRole::Assistant, text)
}

fn numbered(entries: Vec<SessionLogEntry>) -> Vec<(u64, SessionLogEntry)> {
    entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| ((i + 1) as u64, e))
        .collect()
}

fn completed_compaction() -> Vec<(u64, SessionLogEntry)> {
    let turns = vec![
        user("remove", "remove hex"),
        SessionLogEntry::ToolCalls {
            text: "editing".into(),
            thought: None,
            calls: vec![harnx_core::tool::ToolCall::new(
                "edit".into(),
                serde_json::json!({}),
                Some("edit-1".into()),
                None,
            )],
            timestamp: None,
            fence_token: None,
        },
        SessionLogEntry::ToolResults {
            results: vec![harnx_core::session::ToolOutput {
                id: Some("edit-1".into()),
                name: "edit".into(),
                output: serde_json::json!({"ok": true}),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        },
        assistant("hex removed"),
        user("verify", "verify edit"),
        assistant("verified"),
    ];
    let mut entries = vec![user("older", "old request"), assistant("old answer")];
    entries.extend(turns.clone());
    entries.push(SessionLogEntry::Compress {
        prompt: "old work".into(),
    });
    entries.extend(turns);
    numbered(entries)
}

#[test]
fn fold_new_user_messages_since_skips_compaction_copies_by_stable_id() {
    let mut entries = completed_compaction();
    let (messages, cursor) = fold_new_user_messages_since(&entries, Some(7));
    assert!(
        messages.is_empty(),
        "completed retained turns aren't new input"
    );
    assert_eq!(cursor, Some(7));
    entries.push((16, user("export", "fix export")));
    let (messages, cursor) = fold_new_user_messages_since(&entries, Some(7));
    assert_single_input(&messages, cursor, "fix export", 16);
    assert!(fold_new_user_messages_since(&entries, cursor).0.is_empty());
}

#[test]
fn fold_new_user_messages_since_preserves_identical_text_different_id() {
    let mut entries = completed_compaction();
    entries.extend([
        (16, user("new-remove", "remove hex")),
        (17, user("new-verify", "verify edit")),
    ]);
    let (messages, cursor) = fold_new_user_messages_since(&entries, Some(7));
    assert_eq!(
        messages
            .iter()
            .map(|m| (m.content.to_text(), m.log_seq))
            .collect::<Vec<_>>(),
        vec![
            ("remove hex".into(), Some(16)),
            ("verify edit".into(), Some(17))
        ]
    );
    assert_eq!(cursor, Some(17));
}

#[test]
fn fold_new_user_messages_since_keeps_queued_input_before_assistant_barrier_once() {
    let consumed = user("consumed", "remove hex");
    let queued = user("queued", "fix export");
    let entries = numbered(vec![
        consumed.clone(),
        queued.clone(),
        assistant("hex removed"),
        SessionLogEntry::Compress {
            prompt: "summary".into(),
        },
        consumed,
        queued,
        assistant("hex removed"),
    ]);
    let (messages, cursor) = fold_new_user_messages_since(&entries, Some(1));
    assert_single_input(&messages, cursor, "fix export", 2);
    assert!(fold_new_user_messages_since(&entries, cursor).0.is_empty());
}

#[test]
fn fold_new_user_messages_since_applies_edits_and_retractions_to_relogged_entries() {
    let mut entries = completed_compaction();
    entries.extend([
        (16, user("new", "unpatched")),
        (17, user("retracted", "don't run")),
        (
            18,
            SessionLogEntry::EditEntries {
                from: 10,
                to: 10,
                replacements: vec![serde_yaml::to_string(&user("remove", "edited copy")).unwrap()],
            },
        ),
        (
            19,
            SessionLogEntry::EditEntries {
                from: 16,
                to: 16,
                replacements: vec![
                    serde_yaml::to_string(&user("new", "patched new input")).unwrap()
                ],
            },
        ),
        (
            20,
            SessionLogEntry::EditEntries {
                from: 14,
                to: 14,
                replacements: vec![],
            },
        ),
        (
            21,
            SessionLogEntry::EditEntries {
                from: 17,
                to: 17,
                replacements: vec![],
            },
        ),
    ]);
    let (messages, cursor) = fold_new_user_messages_since(&entries, Some(7));
    // Canonical mutation replay assigns replacements their EditEntries sequence.
    assert_single_input(&messages, cursor, "patched new input", 19);
    assert!(fold_new_user_messages_since(&entries, cursor).0.is_empty());
}

#[test]
fn fold_new_user_messages_since_retains_sequence_policy_for_legacy_idless_messages() {
    let entries = numbered(vec![
        message(None, MessageRole::User, "same text"),
        message(None, MessageRole::User, "same text"),
    ]);
    let (messages, cursor) = fold_new_user_messages_since(&entries, Some(1));
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].log_seq, Some(2));
    assert_eq!(cursor, Some(2));
}

fn assert_single_input(
    messages: &[harnx_core::message::Message],
    cursor: Option<u64>,
    text: &str,
    seq: usize,
) {
    let view = messages
        .iter()
        .map(|m| (m.content.to_text(), m.log_seq))
        .collect::<Vec<_>>();
    assert_eq!(
        (view, cursor),
        (vec![(text.to_string(), Some(seq))], Some(seq as u64))
    );
}

#[test]
fn fold_new_user_messages_since_excludes_retracted_messages() {
    let entries = vec![
        (
            1,
            user_entry(
                "msg-1",
                "retracted message",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
            ),
        ),
        (
            2,
            SessionLogEntry::EditEntries {
                from: 1,
                to: 1,
                replacements: vec![],
            },
        ),
        (
            3,
            user_entry(
                "msg-3",
                "valid message",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
            ),
        ),
    ];

    let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);

    assert_eq!(messages.len(), 1, "retracted message must be excluded");
    assert_eq!(messages[0].content.to_text(), "valid message");
    assert_eq!(messages[0].log_seq, Some(3));
    assert_eq!(
        messages[0].log_timestamp,
        Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap())
    );
    assert_eq!(latest_seq, Some(3));
}

#[test]
fn fold_new_user_messages_since_skips_non_user_entries_but_tracks_latest_user_seq() {
    let entries = vec![
        (
            1,
            user_entry(
                "msg-1",
                "first valid",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
            ),
        ),
        (
            2,
            SessionLogEntry::Message {
                id: Some("assistant-2".to_string()),
                role: MessageRole::Assistant,
                content: MessageContent::Text("assistant reply".to_string()),
                timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 2).unwrap()),
                fence_token: None,
            },
        ),
        (
            3,
            user_entry(
                "msg-3",
                "second valid",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
            ),
        ),
    ];

    let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);

    let folded: Vec<_> = messages
        .iter()
        .map(|message| {
            (
                message.content.to_text(),
                message.log_seq,
                message.log_timestamp,
            )
        })
        .collect();
    assert_eq!(
        folded,
        vec![
            (
                "first valid".to_string(),
                Some(1),
                Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap())
            ),
            (
                "second valid".to_string(),
                Some(3),
                Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap())
            ),
        ]
    );
    assert_eq!(
        latest_seq,
        Some(3),
        "latest_seq must be max consumed user-message seq"
    );
}

#[test]
fn fold_new_user_messages_since_cursor_semantics_with_retracts() {
    let entries = vec![
        (
            1,
            user_entry(
                "msg-1",
                "retracted",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
            ),
        ),
        (
            2,
            SessionLogEntry::EditEntries {
                from: 1,
                to: 1,
                replacements: vec![],
            },
        ),
        (
            3,
            user_entry(
                "msg-3",
                "first valid",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
            ),
        ),
        (
            4,
            user_entry(
                "msg-4",
                "second valid",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 4).unwrap(),
            ),
        ),
    ];

    let (messages, latest_seq) = fold_new_user_messages_since(&entries, Some(3));

    assert_eq!(
        messages.len(),
        1,
        "entries with seq <= cursor must be skipped after mutations"
    );
    assert_eq!(messages[0].content.to_text(), "second valid");
    assert_eq!(
        messages[0].log_seq,
        Some(4),
        "returned message must preserve original seq for stamping"
    );
    assert_eq!(
        messages[0].log_timestamp,
        Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 4).unwrap())
    );
    assert_eq!(
        latest_seq,
        Some(4),
        "latest_seq must track max consumed user-message seq"
    );
}
