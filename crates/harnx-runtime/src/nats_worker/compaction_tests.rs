//! Integration tests for manual compaction (CompactRequest/CompactResult).
//!
//! Tests that the worker detects a pending CompactRequest on an idle session,
//! hydrates it, executes compaction, and writes CompactResult.

use super::tests::{env_lock, spawn_test_nats};
use crate::nats_session_log::NatsSessionLog;
use anyhow::Result;
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{CompactOutcome, SessionLogEntry};
use std::sync::{Arc, Mutex};

/// Test the check_already_compacted logic with log entries.
/// This tests the production code by calling the static function directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn already_compacted_detects_new_user_content_after_any_compact_result() -> Result<()> {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return Ok(());
    };

    let client = async_nats::connect(&url).await?;
    let jetstream = async_nats::jetstream::new(client);
    let session_id = crate::nats_worker::new_remote_session_id();
    let log = NatsSessionLog::for_agent(jetstream.clone(), "metis", &session_id).with_replicas(1);

    // Simulate a sequence: user msgs -> CompactRequest -> Compress -> re-logged suffix -> CompactResult
    let compaction_id = "test-already-001";

    // 1. Original user message (will be in the suffix)
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("original user message".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    // 2. CompactRequest
    log.append_event_async(&SessionLogEntry::compact_request(
        compaction_id.to_string(),
        Some("test-user".to_string()),
    ))
    .await?;

    // 3. Compress marker (simulating automatic compaction ran)
    log.append_event_async(&SessionLogEntry::Compress {
        prompt: "summary of earlier conversation".into(),
    })
    .await?;

    // 4. Re-logged suffix message (this is NOT new user content)
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("original user message".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    // 5. CompactResult (with a DIFFERENT compaction_id, to test that ANY CompactResult after Compress counts)
    log.append_event_async(&SessionLogEntry::compact_result(
        "other-compaction-id".to_string(),
        CompactOutcome::Compacted,
    ))
    .await?;

    // 6. Now append actual new user message AFTER CompactResult
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("new user message after compaction".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    // Verify the entries are as expected
    let entries = log.load_events_async().await?;

    // Find CompactResult and verify it exists
    let has_result = entries
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::CompactResult { .. }));
    assert!(has_result, "CompactResult should exist");

    // Call the production code to check if already compacted
    let outcome = super::session_turn::detect_already_compacted(&entries, compaction_id);

    // Should NOT return AlreadyCompacted because there IS new user content after CompactResult
    // (the logic should detect the new user message and return None or not AlreadyCompacted)
    match outcome {
        None => {
            // Expected: no outcome returned because new user content exists
        }
        Some(CompactOutcome::Unchanged(reason)) => {
            // The reason should NOT be AlreadyCompacted since we have new user content
            assert!(
                !matches!(reason, harnx_core::session::UnchangedReason::AlreadyCompacted),
                "Should not return AlreadyCompacted when there is new user content after CompactResult"
            );
        }
        Some(CompactOutcome::Compacted | CompactOutcome::Failed(_)) => {
            panic!("Unexpected outcome: {:?}", outcome);
        }
    }

    // Now test the case without new user content after CompactResult
    let log2 = NatsSessionLog::for_agent(jetstream.clone(), "metis", &format!("{session_id}-2"))
        .with_replicas(1);

    // Same sequence but NO new user message after CompactResult
    log2.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("original user message".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    log2.append_event_async(&SessionLogEntry::compact_request(
        compaction_id.to_string(),
        Some("test-user".to_string()),
    ))
    .await?;

    log2.append_event_async(&SessionLogEntry::Compress {
        prompt: "summary of earlier conversation".into(),
    })
    .await?;

    log2.append_event_async(&SessionLogEntry::compact_result(
        "other-compaction-id".to_string(),
        CompactOutcome::Compacted,
    ))
    .await?;

    let entries2 = log2.load_events_async().await?;
    let outcome2 = super::session_turn::detect_already_compacted(&entries2, compaction_id);

    // Should return AlreadyCompacted because there is NO new user content after CompactResult
    assert!(
        matches!(outcome2, Some(CompactOutcome::Unchanged(harnx_core::session::UnchangedReason::AlreadyCompacted))),
        "Should return AlreadyCompacted when there is no new user content after CompactResult: {:?}",
        outcome2
    );

    // Cleanup
    let _ = child.kill();
    let _ = child.wait();

    Ok(())
}

/// Test TUI `.compact session` command dispatch through NATS.
/// Verifies:
/// 1. First call succeeds with Submitted
/// 2. Second call returns AlreadyInFlight and emits message
/// 3. After CompactResult appended, next call returns NothingToDo
#[test]
fn tui_compact_session_remote_submit_dispatch() -> Result<()> {
    let builder = std::thread::Builder::new()
        .name("tui_compact_session_remote_submit_dispatch".into())
        .stack_size(8 * 1024 * 1024);
    let handler = builder.spawn(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .unwrap()
            .block_on(async {
                let _env_guard = env_lock().await;
                let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
                    return Ok(());
                };

                let mut seeded = super::tests::seed_remote_config(&url);
                let _config_dir =
                    super::tests::TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
                let session_id = crate::nats_worker::new_remote_session_id();

                // Set the agent and session
                seeded
                    .parent_config
                    .set_remote_agent("metis".to_string(), "local".to_string());
                seeded
                    .parent_config
                    .use_session(Some(&session_id))
                    .expect("activate remote session id");

                let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
                let abort = harnx_core::abort::create_abort_signal();

                // Collecting sink to capture events
                #[derive(Default)]
                struct CollectingSink {
                    events: Mutex<Vec<AgentEvent>>,
                }

                impl AgentEventSink for CollectingSink {
                    fn emit(&self, event: AgentEvent) {
                        self.events.lock().unwrap().push(event);
                    }
                }

                let sink = Arc::new(CollectingSink::default());

                // First call: should return Submitted
                harnx_core::sink::with_agent_event_sink(sink.clone(), async {
                    let mut output = Vec::new();
                    crate::commands::run_command_with_output(
                        &global_config,
                        abort.clone(),
                        ".compact session",
                        &mut output,
                    )
                    .await
                    .expect("command succeeds");
                    // Read the log to check CompactRequest was appended
                    let jetstream = {
                        let nats_server = global_config
                            .read()
                            .nats_server("local")
                            .expect("nats server")
                            .clone();
                        drop(global_config.read());
                        crate::config::Config::connect_nats_server(&nats_server)
                            .await
                            .expect("connect")
                    };
                    let jetstream = async_nats::jetstream::new(jetstream);
                    let log =
                        NatsSessionLog::for_agent(jetstream, "metis", &session_id).with_replicas(1);
                    let entries = log.load_events_async().await.expect("load entries");
                    // Find the CompactRequest
                    let has_request = entries
                        .iter()
                        .any(|(_, e)| matches!(e, SessionLogEntry::CompactRequest { .. }));
                    assert!(has_request, "CompactRequest should be appended");
                })
                .await;

                // Second call immediately: should hit AlreadyInFlight
                sink.events.lock().unwrap().clear();
                harnx_core::sink::with_agent_event_sink(sink.clone(), async {
                    let mut output = Vec::new();
                    crate::commands::run_command_with_output(
                        &global_config,
                        abort.clone(),
                        ".compact session",
                        &mut output,
                    )
                    .await
                    .expect("command succeeds");
                })
                .await;

                // Should have emitted "Compaction already in progress"
                {
                    let events = sink.events.lock().unwrap();
                    let has_already_in_progress = events.iter().any(|e| {
                        matches!(
                            e,
                            AgentEvent::Session(SessionEvent::Generic { text, .. })
                                if text == "Compaction already in progress"
                        )
                    });
                    assert!(
                        has_already_in_progress,
                        "should emit 'Compaction already in progress', got {:?}",
                        events
                    );
                }

                // Now append a CompactResult to simulate worker completion
                {
                    let jetstream = {
                        let nats_server = global_config
                            .read()
                            .nats_server("local")
                            .expect("nats server")
                            .clone();
                        drop(global_config.read());
                        crate::config::Config::connect_nats_server(&nats_server)
                            .await
                            .expect("connect")
                    };
                    let jetstream = async_nats::jetstream::new(jetstream);
                    let log =
                        NatsSessionLog::for_agent(jetstream, "metis", &session_id).with_replicas(1);
                    log.append_event_async(&SessionLogEntry::compact_result(
                        "test-compaction-id".to_string(),
                        CompactOutcome::Compacted,
                    ))
                    .await
                    .expect("append CompactResult");
                }

                // Third call: should hit NothingToDo
                sink.events.lock().unwrap().clear();
                harnx_core::sink::with_agent_event_sink(sink.clone(), async {
                    let mut output = Vec::new();
                    crate::commands::run_command_with_output(
                        &global_config,
                        abort.clone(),
                        ".compact session",
                        &mut output,
                    )
                    .await
                    .expect("command succeeds");
                })
                .await;

                let events = sink.events.lock().unwrap();
                let has_nothing_to_compact = events.iter().any(|e| {
                    matches!(
                        e,
                        AgentEvent::Session(SessionEvent::Generic { text, .. })
                            if text == "Nothing to compact"
                    )
                });
                assert!(
                    has_nothing_to_compact,
                    "should emit 'Nothing to compact', got {:?}",
                    events
                );

                // Cleanup
                let _ = child.kill();
                let _ = child.wait();

                Ok(())
            })
    })?;
    handler.join().unwrap()
}

/// Test detect_already_compacted when Compress marker exists after CompactRequest.
/// This exercises the branch where automatic compaction and manual request coalesce,
/// and the fallback branch in execute_manual_compaction that checks this condition.
#[test]
fn detect_already_compacted_returns_compacted_when_compress_after_request() {
    use harnx_core::session::{CompactOutcome, SessionLogEntry, UnchangedReason};

    // Entries with CompactRequest -> Compress -> CompactResult
    let entries: Vec<(u64, SessionLogEntry)> = vec![
        (
            1u64,
            SessionLogEntry::compact_request("test-compaction-id".to_string(), Some("user".into())),
        ),
        (
            2u64,
            SessionLogEntry::Compress {
                prompt: "summary".into(),
            },
        ),
        (
            3u64,
            SessionLogEntry::compact_result(
                "auto-compaction-id".to_string(),
                CompactOutcome::Compacted,
            ),
        ),
    ];

    // Should return AlreadyCompacted because Compress exists after our request
    let outcome = super::session_turn::detect_already_compacted(&entries, "test-compaction-id");
    assert!(
        matches!(
            outcome,
            Some(CompactOutcome::Unchanged(UnchangedReason::AlreadyCompacted))
        ),
        "Expected AlreadyCompacted when Compress marker after request, got {:?}",
        outcome
    );
}

/// Test detect_already_compacted returns None when no Compress marker after request.
/// This exercises the fallback branch in execute_manual_compaction that runs compaction
/// when automatic compaction didn't coalesce with the manual request.
#[test]
fn detect_already_compacted_returns_none_when_no_compress_after_request() {
    use harnx_core::session::SessionLogEntry;

    // Entries with CompactRequest but no Compress yet
    let entries: Vec<(u64, SessionLogEntry)> = vec![(
        1u64,
        SessionLogEntry::compact_request("test-compaction-id".to_string(), Some("user".into())),
    )];

    // Should return None because no Compress after our request
    let outcome = super::session_turn::detect_already_compacted(&entries, "test-compaction-id");
    assert!(
        outcome.is_none(),
        "Expected None when no Compress marker after request, got {:?}",
        outcome
    );
}

/// Test detect_already_compacted returns None when there's new user content after compaction.
/// This ensures we don't incorrectly return AlreadyCompacted when new work exists.
#[test]
fn detect_already_compacted_returns_none_when_new_user_content_after_compaction() {
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::{CompactOutcome, SessionLogEntry};

    // Entries with CompactRequest -> Compress -> CompactResult -> New User Message
    let entries: Vec<(u64, SessionLogEntry)> = vec![
        (
            1u64,
            SessionLogEntry::compact_request("test-compaction-id".to_string(), Some("user".into())),
        ),
        (
            2u64,
            SessionLogEntry::Compress {
                prompt: "summary".into(),
            },
        ),
        (
            3u64,
            SessionLogEntry::compact_result(
                "auto-compaction-id".to_string(),
                CompactOutcome::Compacted,
            ),
        ),
        (
            4u64,
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("new user message".into()),
                timestamp: None,
                fence_token: None,
            },
        ),
    ];

    // Should return None because there's new user content after compaction
    let outcome = super::session_turn::detect_already_compacted(&entries, "test-compaction-id");
    assert!(
        outcome.is_none(),
        "Expected None when new user content after compaction, got {:?}",
        outcome
    );
}
