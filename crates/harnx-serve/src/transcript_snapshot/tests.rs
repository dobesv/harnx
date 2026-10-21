use super::*;
use crate::session_actor::{SessionCapabilities, SessionInfo, SessionState};

const FIRST_SUMMARY: &str =
    "The session log stores tool calls and results separately. No messages were kept.";
const LATEST_SUMMARY: &str = "Earlier log inspection is archived. The latest prompt, verification round and final reply remain verbatim.";

pub(crate) fn fixture(through: u64) -> Vec<(u64, SessionLogEntry)> {
    let entries: Vec<(u64, SessionLogEntry)> = serde_json::from_str(include_str!(
        "../../../harnx-core/tests/fixtures/compaction-history.json"
    ))
    .expect("shared compaction fixture");
    entries
        .into_iter()
        .filter(|(seq, _)| *seq <= through)
        .collect()
}

fn snapshot(through: u64) -> HistorySnapshot {
    HistorySnapshot::from_entries(&fixture(through), &Session::default(), false).unwrap()
}

fn message_rows(snapshot: &HistorySnapshot) -> Vec<Value> {
    snapshot
        .messages
        .iter()
        .map(|message| serde_json::to_value(message).unwrap())
        .collect()
}

fn info(snapshot: HistorySnapshot) -> SessionInfo {
    SessionInfo {
        state: SessionState::Idle,
        worker_active: false,
        history_snapshot: snapshot.messages,
        compaction: snapshot.compaction,
        history_warnings: Vec::new(),
        capabilities: SessionCapabilities {
            can_prompt: true,
            can_cancel: false,
            supports_snapshot: true,
        },
    }
}

#[test]
fn zero_kept_snapshot_contains_summary_instead_of_blank_history() {
    let snapshot = snapshot(6);
    assert_eq!(
        message_rows(&snapshot),
        vec![json!({
            "id": crate::ag_ui_sync::wire_message_id("compaction:5"),
            "role": "assistant", "content": FIRST_SUMMARY,
        })]
    );
    assert_eq!(snapshot.compaction.unwrap().kept_count, Some(0));
}

#[test]
fn prompt_tail_and_later_activity_follow_summary_once() {
    let snapshot = snapshot(23);
    let rows = message_rows(&snapshot);
    let roles: Vec<_> = rows
        .iter()
        .map(|row| row["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        [
            "assistant",
            "user",
            "assistant",
            "tool",
            "assistant",
            "user",
            "assistant"
        ]
    );
    assert_visible_text(&rows);
    assert_retained_tool_round(&rows);
}

fn assert_visible_text(rows: &[Value]) {
    let contents: Vec<_> = rows
        .iter()
        .filter(|row| row["role"] != "tool")
        .map(|row| row["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        contents,
        [
            LATEST_SUMMARY,
            "Implement the shared transcript view and verify paging.",
            "Check the transcript view tests.",
            "The shared view is implemented. Paging tests passed.",
            "Show the previous compaction's history.",
            "The earlier window contains the original prompt and both tool rounds.",
        ]
    );
}

fn assert_retained_tool_round(rows: &[Value]) {
    assert_eq!(rows[2]["toolCalls"][0]["id"], "retained-round");
    assert_eq!(rows[3]["toolCallId"], "retained-round");
    assert!(rows[3]["content"]
        .as_str()
        .unwrap()
        .contains("Transcript view tests passed."));
}

#[test]
fn synthetic_assistant_has_string_content_and_stable_uuid_linked_from_metadata() {
    for through in [6, 23] {
        let snapshot = snapshot(through);
        let metadata = snapshot.compaction.as_ref().unwrap();
        let row = &message_rows(&snapshot)[0];
        assert_eq!(row["role"], "assistant");
        assert_eq!(row["content"], metadata.summary);
        assert_eq!(row["id"], metadata.message_id);
    }
}

#[test]
fn compaction_uuid_uses_existing_wire_id_namespace() {
    let first = snapshot(23).compaction.unwrap();
    let second = snapshot(23).compaction.unwrap();
    assert_eq!(first.message_id, second.message_id);
    assert_eq!(
        first.message_id,
        crate::ag_ui_sync::wire_message_id("compaction:15").to_string()
    );
    assert!(uuid::Uuid::parse_str(&first.message_id).is_ok());
}

#[test]
fn session_get_omits_metadata_until_compacted() {
    let result = crate::ag_ui_rpc::session_get_result(&info(snapshot(3)), "nats");
    assert!(result.get("compaction").is_none());
    assert_eq!(result["history_snapshot"].as_array().unwrap().len(), 2);
}

#[test]
fn session_get_serializes_compaction_metadata_without_unknown_counts() {
    let result = crate::ag_ui_rpc::session_get_result(&info(snapshot(23)), "nats");
    assert_eq!(
        result["compaction"],
        json!({
            "message_id": crate::ag_ui_sync::wire_message_id("compaction:15"),
            "compaction_seq": 15, "summary": LATEST_SUMMARY,
            "from_seq": 6, "to_seq": 14, "earlier_compaction": 5,
        })
    );
    assert_eq!(
        result["history_snapshot"][0]["id"],
        result["compaction"]["message_id"]
    );
}

#[test]
fn live_snapshot_carries_same_metadata_and_messages_as_session_get() {
    for through in [3, 6, 23] {
        let snapshot = snapshot(through);
        let result = crate::ag_ui_rpc::session_get_result(&info(snapshot.clone()), "nats");
        let events = snapshot.events();
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap(),
            json!({
                "type": "CUSTOM", "name": "session_compaction", "value": { "compaction": result["compaction"] },
            })
        );
        assert_eq!(
            serde_json::to_value(&events[1]).unwrap(),
            json!({
                "type": "MESSAGES_SNAPSHOT", "messages": result["history_snapshot"],
            })
        );
    }
}

#[test]
fn initial_attach_frames_carry_metadata_before_snapshot() {
    let snapshot = snapshot(6);
    let frames =
        crate::ag_ui_attach::initial_attach_frame(snapshot.clone(), Vec::new(), true).unwrap();
    let events: Vec<Value> = std::str::from_utf8(&frames)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events,
        snapshot
            .events()
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect::<Vec<_>>()
    );
}

#[test]
fn prompted_attach_still_omits_snapshot_and_metadata() {
    assert!(crate::ag_ui_attach::initial_attach_frame(snapshot(23), Vec::new(), false).is_none());
}

#[test]
fn completed_event_adds_optional_fields_and_then_matching_snapshot() {
    use harnx_core::event::SessionEvent;
    let snapshot = snapshot(6);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = crate::ag_ui::AgUiSink::new(tx, ag_ui_core::types::ids::MessageId::random());
    sink.emit_compacting_completed(
        SessionEvent::CompactingCompleted {
            compaction_id: Some("keep-nothing".into()),
            outcome: harnx_core::session::CompactOutcome::Compacted,
        },
        Some(snapshot.clone()),
    );
    assert_eq!(
        serde_json::to_value(rx.try_recv().unwrap()).unwrap(),
        json!({
            "type": "CUSTOM", "name": "session_compacting_completed", "value": {
                "compaction_id": "keep-nothing", "outcome": { "status": "compacted" },
                "summary": FIRST_SUMMARY, "from_seq": 1, "to_seq": 4, "compaction_seq": 5,
            },
        })
    );
    let remaining: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(remaining, snapshot.events());
}

#[test]
fn invalid_log_fails_projection_instead_of_showing_archived_messages() {
    let mut entries = fixture(23);
    entries.swap(0, 1);
    assert!(HistorySnapshot::from_entries(&entries, &Session::default(), false).is_err());
}

#[test]
fn rewinding_before_compaction_removes_summary_and_metadata() {
    let mut entries = fixture(23);
    entries.push((24, SessionLogEntry::Rewind { after_seq: 2 }));
    let actual = HistorySnapshot::from_entries(&entries, &Session::default(), false).unwrap();
    assert_eq!(actual, snapshot(3));
}

#[test]
fn edited_replacements_keep_order_even_with_shared_mutation_sequences() {
    let mut entries = fixture(23);
    let replacements = ["replacement one", "replacement two"]
        .map(replacement_prompt)
        .to_vec();
    entries.push((
        24,
        SessionLogEntry::EditEntries {
            from: 21,
            to: 21,
            replacements,
        },
    ));
    let actual = HistorySnapshot::from_entries(&entries, &Session::default(), false).unwrap();
    let rows = message_rows(&actual);
    let contents: Vec<_> = rows[5..]
        .iter()
        .map(|row| row["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        contents,
        [
            "replacement one",
            "replacement two",
            "The earlier window contains the original prompt and both tool rounds."
        ]
    );
}

fn replacement_prompt(text: &str) -> String {
    let mut entry = fixture(21).pop().unwrap().1;
    let SessionLogEntry::Message { content, .. } = &mut entry else {
        panic!("fixture prompt")
    };
    *content = harnx_core::message::MessageContent::Text(text.into());
    serde_json::to_string(&entry).unwrap()
}

#[test]
fn compacted_snapshot_preserves_inflight_tool_calls_while_worker_is_active() {
    let entries = fixture(17);
    let active = HistorySnapshot::from_entries(&entries, &Session::default(), true).unwrap();
    let idle = HistorySnapshot::from_entries(&entries, &Session::default(), false).unwrap();
    assert_eq!((active.messages.len(), idle.messages.len()), (3, 4));
    assert_eq!(active.compaction, idle.compaction);
}

#[test]
fn empty_archive_metadata_preserves_reversed_bounds() {
    let entries = vec![(
        1,
        SessionLogEntry::Compress {
            prompt: "summary".into(),
        },
    )];
    let actual = HistorySnapshot::from_entries(&entries, &Session::default(), false).unwrap();
    let metadata = actual.compaction.unwrap();
    assert_eq!((metadata.from_seq, metadata.to_seq), (1, 0));
}
