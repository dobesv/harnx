use super::*;

#[test]
fn interaction_metadata_is_readable_but_not_transcript_or_model_output() {
    let metadata: Vec<SessionLogEntry> = serde_json::from_value(json!([
        {"type": "tool_call_completed", "tool_round_seq": 7, "tool_call_id": "a",
         "result": {"id": "a", "name": "read", "output": "partial result"},
         "private_snapshot": "private-result-ref", "fence_token": 4},
        {"type": "interaction_requested", "tool_round_seq": 7, "tool_call_id": "b",
         "interaction_id": "interaction", "producer": "middleware:confirm", "step_path": [],
         "key": "keep", "kind": "confirm", "payload": {"text": "Keep?"}, "summary": "Keep?",
         "chain_version": "v1", "checkpoint": "private-checkpoint-ref", "fence_token": 4},
        {"type": "suspension_ready", "tool_round_seq": 7, "pending": ["interaction"], "fence_token": 4},
        {"type": "interaction_response", "request_seq": 9, "tool_call_id": "b",
         "interaction_id": "interaction", "response": {"action": "approve", "data": {}}, "fence_token": 5}
    ])).unwrap();
    let entries: Vec<_> = metadata
        .into_iter()
        .enumerate()
        .map(|(i, entry)| (8 + i as u64, entry))
        .collect();
    assert_replay(&entries, vec![]);
    let raw: Vec<_> = entries
        .into_iter()
        .map(|(seq, entry)| (seq as usize, entry))
        .collect();
    let session =
        crate::config::session::replay_log_entries_for_external(&raw, "interaction-reader", false)
            .unwrap();
    assert!(
        session.messages.is_empty(),
        "control records must not become model messages"
    );
}
