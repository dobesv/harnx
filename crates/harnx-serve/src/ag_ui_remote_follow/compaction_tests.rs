use super::*;
use crate::transcript_snapshot::{tests::fixture, HistorySnapshot};
use harnx_core::event::{AgentEvent, SessionEvent};
use harnx_core::session::{CompactOutcome, Session};

#[test]
fn remote_completion_hydrates_summary_and_metadata_from_shared_fixture() {
    for through in [6, 23] {
        let history = fixture(through);
        let base = Session::default();
        let snapshot = HistorySnapshot::from_entries(&history, &base, false).unwrap();
        let events = pending::completion_events(&history, "fixture", Some(&base)).unwrap();
        assert_eq!(
            normalize_tool_row_ids(&events[..2]),
            normalize_tool_row_ids(&snapshot.events())
        );
    }
}

#[test]
fn remote_compaction_advisory_emits_completed_metadata_then_snapshot() {
    let history = fixture(6);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let mut forwarder = AdvisoryForwarder::new(tx);
    forwarder.emit_with_history(
        AgentEvent::Session(SessionEvent::CompactingCompleted {
            compaction_id: Some("keep-nothing".into()),
            outcome: CompactOutcome::Compacted,
        }),
        &history,
    );
    let completed = serde_json::to_value(forwarder.event_rx.try_recv().unwrap()).unwrap();
    assert_eq!(completed["value"]["compaction_seq"], 5);
    assert_remote_snapshot(&mut forwarder, &history);
}

fn assert_remote_snapshot(forwarder: &mut AdvisoryForwarder, history: &[(u64, SessionLogEntry)]) {
    let snapshot = HistorySnapshot::from_entries(history, &Session::default(), true).unwrap();
    let events: Vec<_> = std::iter::from_fn(|| forwarder.event_rx.try_recv().ok()).collect();
    assert_eq!(events, snapshot.events());
}

// Legacy tool rows have random message IDs; call identity and content stay exact.
fn normalize_tool_row_ids(events: &[Event]) -> Vec<serde_json::Value> {
    events.iter().map(normalize_event_tool_ids).collect()
}

fn normalize_event_tool_ids(event: &Event) -> serde_json::Value {
    let mut value = serde_json::to_value(event).unwrap();
    if let Some(messages) = value["messages"].as_array_mut() {
        messages.iter_mut().for_each(normalize_tool_row_id);
    }
    value
}

fn normalize_tool_row_id(message: &mut serde_json::Value) {
    if message["role"] == "tool" || message.get("toolCalls").is_some() {
        message.as_object_mut().unwrap().remove("id");
    }
}

#[test]
fn remote_compaction_projection_respects_session_instructions() {
    let mut history = fixture(6);
    let entry = serde_json::from_value(serde_json::json!({
        "type": "message", "role": "system", "content": "legacy instructions",
    }))
    .unwrap();
    history.push((7, entry));
    let base = Session {
        agent_instructions: "configured instructions".into(),
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let mut forwarder = AdvisoryForwarder::new(tx).with_session_base(Some(base.clone()));
    forwarder.emit_with_history(compacted_event(), &history);
    forwarder.event_rx.try_recv().unwrap();
    let expected = HistorySnapshot::from_entries(&history, &base, true).unwrap();
    let events: Vec<_> = std::iter::from_fn(|| forwarder.event_rx.try_recv().ok()).collect();
    assert_eq!(expected.messages.len(), 1);
    assert_eq!(events, expected.events());
}

fn compacted_event() -> AgentEvent {
    AgentEvent::Session(SessionEvent::CompactingCompleted {
        compaction_id: Some("keep-nothing".into()),
        outcome: CompactOutcome::Compacted,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compaction_refreshes_history_without_projecting_after_advisory_boundary() {
    harnx_core::require_nextest();
    let sandbox = crate::test_support::TestConfigSandbox::new();
    let config = sandbox.config();
    if !crate::test_support::ensure_test_nats().await {
        return;
    }
    let storage_key = format!("remote-compaction-{}", uuid::Uuid::new_v4());
    let jetstream = crate::serve_nats_jetstream(&config, harnx_runtime::config::LOCAL_CLUSTER_KEY)
        .await
        .unwrap();
    let client = crate::serve_nats_client(&config, harnx_runtime::config::LOCAL_CLUSTER_KEY)
        .await
        .unwrap();
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(
        jetstream.clone(),
        storage_key.clone(),
    )
    .with_replicas(1);
    for (_, entry) in fixture(3) {
        log.append_event_async(&entry).await.unwrap();
    }
    let mut stream = SessionEventStream::attach(jetstream, client, &storage_key)
        .await
        .unwrap();
    append_compaction_after_attach(&log, 23).await;
    assert_eq!(stream.last_applied_seq(), 3);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut forwarder = AdvisoryForwarder::new(tx);
    forwarder
        .forward(
            &mut stream,
            Some(AdvisoryEnvelope::new(6, compacted_event())),
            &storage_key,
        )
        .await
        .unwrap();
    assert_eq!(stream.last_applied_seq(), 23);
    assert_refreshed_remote_events(&mut rx);
}

async fn append_compaction_after_attach(
    log: &harnx_runtime::nats_session_log::NatsSessionLog,
    through: u64,
) {
    for (_, entry) in fixture(through).into_iter().filter(|(seq, _)| *seq > 3) {
        log.append_event_async(&entry).await.unwrap();
    }
}

fn assert_refreshed_remote_events(rx: &mut tokio::sync::mpsc::Receiver<QueuedEvent>) {
    let completed = serde_json::to_value(rx.try_recv().unwrap().event).unwrap();
    assert_eq!(completed["value"]["compaction_seq"], 5);
    let expected = HistorySnapshot::from_entries(&fixture(6), &Session::default(), true).unwrap();
    for expected in expected.events() {
        assert_eq!(rx.try_recv().unwrap().event, expected);
    }
}

mod refresh_failure;
