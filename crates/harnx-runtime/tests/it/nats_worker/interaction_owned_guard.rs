//! Mixed-version workers must not turn parked effects into legacy orphan repair.
use super::*;
use anyhow::Context;
use tokio_util::task::AbortOnDropHandle;

#[path = "interaction_owned_guard/checkpoint_reader.rs"]
mod checkpoint_reader;
#[path = "interaction_owned_guard/fixture.rs"]
mod fixture;
#[path = "interaction_owned_guard/records.rs"]
mod records;
use fixture::{Fixture, WARNINGS};

#[derive(Clone, Copy, Debug)]
enum Case {
    InteractionRecords,
    SuspensionReady,
    RoundStart,
    StoreError,
    InvalidStore,
    MissingBucket,
    MissingKey,
    #[cfg_attr(
        not(feature = "fault-injection"),
        expect(
            dead_code,
            reason = "T2.1 handoff fixture requires the old-worker fault mode"
        )
    )]
    Handoff,
    HandoffCommitted,
    UnmarkedHandoff,
    ErrorHandoff,
}

impl Case {
    fn parks(self) -> bool {
        matches!(self, Self::StoreError | Self::InvalidStore | Self::Handoff)
    }
    fn has_results(self) -> bool {
        matches!(
            self,
            Self::Handoff | Self::HandoffCommitted | Self::UnmarkedHandoff | Self::ErrorHandoff
        )
    }
}

async fn exercise(case: Case) -> Result<()> {
    let fixture = Fixture::new(case).await?;
    let before = fixture.log.load_events_latest_async().await?;
    let metrics = harnx_runtime::nats_metrics::snapshot();
    if case.parks() {
        // Direct agent-loop entry bypasses daemon preflight; it still cannot
        // repair or replay the seeded journal, dispatch handoff or call model.
        fixture.direct_run().await?;
        assert_eq!(fixture.log.load_events_latest_async().await?, before);
        assert_eq!(fixture.models.load(Ordering::SeqCst), 0);
        assert!(!WARNINGS.messages.lock().unwrap().is_empty());
        WARNINGS.messages.lock().unwrap().clear();
    }
    let daemon = fixture.spawn_daemon().await?;
    activate_session(&fixture.js, fixture.id).await?;
    wait_for_worker_daemon_idle(&fixture.js, fixture.id, metrics.lease_acquisitions).await?;
    let after = fixture.log.load_events_latest_async().await?;
    if case.parks() {
        assert_eq!(
            after, before,
            "parked invocation must have no synthetic/replayed results or terminal records"
        );
        assert_eq!(fixture.models.load(Ordering::SeqCst), 0);
        assert_eq!(
            fixture.tools.load(Ordering::SeqCst),
            0,
            "safe tool must not rerun"
        );
        let warnings = WARNINGS.messages.lock().unwrap();
        assert!(
            warnings
                .iter()
                .any(|line| line.contains(if matches!(case, Case::Handoff) {
                    "newer worker needed"
                } else {
                    "checkpoint recovery deferred"
                })),
            "{warnings:?}"
        );
        if matches!(case, Case::StoreError | Case::InvalidStore) {
            assert!(warnings
                .iter()
                .any(|line| line.contains("checkpoint recovery deferred")));
        }
        let metrics_after = harnx_runtime::nats_metrics::snapshot();
        assert_eq!(metrics_after.resumes, metrics.resumes);
        assert_eq!(
            metrics_after.interrupt_errors_synthesized,
            metrics.interrupt_errors_synthesized
        );
        assert!(matches!(
            reconstruct_state_from_nats(&after).turn_status,
            TurnStatus::InFlightResumable { .. }
        ));
    } else {
        assert_eq!(
            fixture.models.load(Ordering::SeqCst),
            usize::from(!matches!(case, Case::HandoffCommitted)),
            "only unmarked/ordinary recovery can call the model: {after:#?}"
        );
        if matches!(case, Case::HandoffCommitted) {
            // Handoff has no final model message. Its physical TurnEnd, not
            // transcript-only reconstruction, is the invocation terminal.
            assert!(after
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. })));
            assert_eq!(
                after
                    .iter()
                    .filter(|(_, entry)| matches!(entry, SessionLogEntry::HandoffCommitted { .. }))
                    .count(),
                1
            );
        } else {
            assert_eq!(
                reconstruct_state_from_nats(&after).turn_status,
                TurnStatus::Idle
            );
        }
        assert_recovered_slots(case, &fixture, &after);
    }
    assert!(!after.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Cancel { .. } | SessionLogEntry::Error { .. }
    )));
    let original = before
        .iter()
        .find(|(_, entry)| {
            matches!(entry,
        SessionLogEntry::Message { id: Some(id), .. } if id == "original-prompt")
        })
        .unwrap();
    assert!(
        after.contains(original),
        "original admitted prompt must survive recovery"
    );
    drop(daemon);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_missing_private_log_data_recovers_interrupted() -> Result<()> {
    exercise(Case::InteractionRecords).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_suspension_ready_alone_reconciles() -> Result<()> {
    exercise(Case::SuspensionReady).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_round_start_only_reconciles() -> Result<()> {
    exercise(Case::RoundStart).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_store_error_fails_closed() -> Result<()> {
    exercise(Case::StoreError).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_confirmed_missing_bucket_recovers() -> Result<()> {
    exercise(Case::MissingBucket).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_confirmed_missing_key_recovers() -> Result<()> {
    exercise(Case::MissingKey).await
}
#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_t2_1_marked_handoff_parks() -> Result<()> {
    let key = storage_key("interaction-owned-guard");
    harnx_runtime::fault_injection::simulate_t2_1_worker(&key, true);
    let result = exercise(Case::Handoff).await;
    harnx_runtime::fault_injection::simulate_t2_1_worker(&key, false);
    result
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_later_handoff_commit_recovers() -> Result<()> {
    exercise(Case::HandoffCommitted).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_unmarked_handoff_recovers() -> Result<()> {
    exercise(Case::UnmarkedHandoff).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_error_marked_handoff_recovers() -> Result<()> {
    exercise(Case::ErrorHandoff).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_nonmissing_store_open_error_parks() -> Result<()> {
    exercise(Case::InvalidStore).await
}

fn assert_recovered_slots(case: Case, fixture: &Fixture, entries: &[(u64, SessionLogEntry)]) {
    if case.has_results() {
        return;
    }
    let expected_runs = match case {
        Case::InteractionRecords => 0,
        Case::RoundStart => 2,
        _ => 1,
    };
    assert_eq!(
        fixture.tools.load(Ordering::SeqCst),
        expected_runs,
        "recovery must use the classified safe rerun set: {entries:#?}"
    );
    for id in ["saved", "unknown", "rerun"] {
        assert_eq!(
            count_tool_results_with_id(entries, id),
            1,
            "each original call needs exactly one result: {id}"
        );
    }
    if matches!(case, Case::InteractionRecords) {
        for (_, entry) in entries {
            if let SessionLogEntry::ToolResults { results, .. } = entry {
                for result in results
                    .iter()
                    .filter(|r| matches!(r.id.as_deref(), Some("saved" | "rerun")))
                {
                    assert_eq!(result.output.get("is_error"), Some(&json!(true)));
                    assert!(result
                        .output
                        .to_string()
                        .contains("interrupted, outcome unknown"));
                }
            }
        }
    }
}
