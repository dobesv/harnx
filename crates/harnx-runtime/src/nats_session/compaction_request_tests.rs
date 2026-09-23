//! Integration tests for compaction request submission.
//!
//! Mirrors the interrupt_tests.rs structure for the compaction workflow.

use super::compaction_request::*;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_test_common::spawn_nats_server;
use crate::nats_worker::SessionActivationRoute;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{CompactOutcome, SessionLogEntry, UnchangedReason};

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn compact_request(id: &str) -> SessionLogEntry {
    SessionLogEntry::compact_request(id, Some("test".into()))
}

fn compact_result(id: &str, outcome: CompactOutcome) -> SessionLogEntry {
    SessionLogEntry::compact_result(id, outcome)
}

fn request(session: &str, id: &str) -> CompactionRequest {
    CompactionRequest {
        session_id: session.into(),
        cluster: "local".into(),
        replicas: 1,
        compaction_id: id.into(),
        requested_by: Some("test".into()),
    }
}

/// Normal session (idle, no pending compaction) gets a CompactRequest appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_session_gets_compact_request_appended() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new(js.clone(), "idle-s").with_replicas(1);

    // Add some history
    log.append_event_async(&user("first turn")).await.unwrap();

    let outcome = request_compaction_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("idle-s", "comp-1"),
    )
    .await
    .unwrap();

    assert!(matches!(
        outcome,
        CompactSubmit::Submitted { compaction_id } if compaction_id == "comp-1"
    ));

    // Verify the entry was appended
    let entries = log.load_events_async().await.unwrap();
    assert!(entries.iter().any(|(_, e)| matches!(
        e,
        SessionLogEntry::CompactRequest { compaction_id, .. } if compaction_id == "comp-1"
    )));
}

/// When tail is an unresolved CompactRequest, skip appending and return AlreadyInFlight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_request_when_tail_is_unresolved_request_is_skipped() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new(js.clone(), "dup-s").with_replicas(1);

    // Pre-populate with existing request
    log.append_event_async(&user("first")).await.unwrap();
    log.append_event_async(&compact_request("comp-existing"))
        .await
        .unwrap();

    let outcome = request_compaction_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("dup-s", "comp-new"),
    )
    .await
    .unwrap();

    assert!(matches!(
        outcome,
        CompactSubmit::AlreadyInFlight { compaction_id } if compaction_id == "comp-existing"
    ));

    // Verify no new entry appended
    let entries = log.load_events_async().await.unwrap();
    let request_count = entries
        .iter()
        .filter(|(_, e)| matches!(e, SessionLogEntry::CompactRequest { .. }))
        .count();
    assert_eq!(request_count, 1);
}

/// When tail is CompactResult with Compacted or Unchanged, skip appending and return NothingToDo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_request_when_tail_is_compacted_or_unchanged_result_is_skipped() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new(js.clone(), "done-s").with_replicas(1);

    // Pre-populate with compaction that has already completed
    log.append_event_async(&user("first")).await.unwrap();
    log.append_event_async(&compact_request("comp-1"))
        .await
        .unwrap();
    log.append_event_async(&compact_result("comp-1", CompactOutcome::Compacted))
        .await
        .unwrap();

    let outcome = request_compaction_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("done-s", "comp-new"),
    )
    .await
    .unwrap();

    assert!(matches!(
        outcome,
        CompactSubmit::NothingToDo {
            outcome: CompactOutcome::Compacted
        }
    ));

    // Verify no new entry appended
    let entries = log.load_events_async().await.unwrap();
    let request_count = entries
        .iter()
        .filter(|(_, e)| matches!(e, SessionLogEntry::CompactRequest { .. }))
        .count();
    assert_eq!(request_count, 1);
}

/// When tail is CompactResult with Unchanged, also skip appending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_result_skips_append() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new(js.clone(), "unchanged-s").with_replicas(1);

    log.append_event_async(&user("first")).await.unwrap();
    log.append_event_async(&compact_request("comp-1"))
        .await
        .unwrap();
    log.append_event_async(&compact_result(
        "comp-1",
        CompactOutcome::Unchanged(UnchangedReason::NoUserMessages),
    ))
    .await
    .unwrap();

    let outcome = request_compaction_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("unchanged-s", "comp-new"),
    )
    .await
    .unwrap();

    assert!(matches!(
        outcome,
        CompactSubmit::NothingToDo {
            outcome: CompactOutcome::Unchanged(UnchangedReason::NoUserMessages)
        }
    ));
}

/// When tail is CompactResult with Failed, allow re-submission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_after_failed_result_is_allowed() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new(js.clone(), "failed-s").with_replicas(1);

    // Pre-populate with a failed compaction
    log.append_event_async(&user("first")).await.unwrap();
    log.append_event_async(&compact_request("comp-old"))
        .await
        .unwrap();
    log.append_event_async(&compact_result(
        "comp-old",
        CompactOutcome::Failed("some error".into()),
    ))
    .await
    .unwrap();

    let outcome = request_compaction_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("failed-s", "comp-new"),
    )
    .await
    .unwrap();

    assert!(matches!(
        outcome,
        CompactSubmit::Submitted { compaction_id } if compaction_id == "comp-new"
    ));

    // Verify the new request was appended
    let entries = log.load_events_async().await.unwrap();
    assert!(entries.iter().any(|(_, e)| matches!(
        e,
        SessionLogEntry::CompactRequest { compaction_id, .. } if compaction_id == "comp-new"
    )));
}

/// Verify that decide_compact_submit handles all expected entry types correctly.
#[test]
fn decide_unit_tests_cover_all_cases() {
    use super::compaction_request::decide_compact_submit;

    // Empty entries = proceed (None)
    let entries: Vec<(u64, SessionLogEntry)> = vec![];
    assert!(decide_compact_submit(&entries).is_none());

    // CompactRequest = AlreadyInFlight
    let entries = vec![(1u64, compact_request("comp-1"))];
    assert!(matches!(
        decide_compact_submit(&entries),
        Some(CompactSubmit::AlreadyInFlight { .. })
    ));

    // CompactResult Compacted = NothingToDo
    let entries = vec![(1u64, compact_result("comp-1", CompactOutcome::Compacted))];
    assert!(matches!(
        decide_compact_submit(&entries),
        Some(CompactSubmit::NothingToDo { .. })
    ));

    // CompactResult Unchanged = NothingToDo
    let entries = vec![(
        1u64,
        compact_result(
            "comp-1",
            CompactOutcome::Unchanged(UnchangedReason::NoUserMessages),
        ),
    )];
    assert!(matches!(
        decide_compact_submit(&entries),
        Some(CompactSubmit::NothingToDo { .. })
    ));

    // CompactResult Failed = proceed (None)
    let entries = vec![(
        1u64,
        compact_result("comp-1", CompactOutcome::Failed("error".into())),
    )];
    assert!(decide_compact_submit(&entries).is_none());

    // User message = proceed (None)
    let entries = vec![(1u64, user("hello"))];
    assert!(decide_compact_submit(&entries).is_none());

    // Compress = proceed (None)
    let entries = vec![(
        1u64,
        SessionLogEntry::Compress {
            prompt: "summary".into(),
        },
    )];
    assert!(decide_compact_submit(&entries).is_none());
}
