//! Guarded-entry revision races for the manual expired-session cleanup pass
//! (#2923 review T02): the canonical record is updated or removed after
//! admission but before the deletion cascade's expected-revision check. The
//! rejection must mutate nothing and refund the admission, so a later
//! eligible candidate still enters under `--max 1`; the late final-CAS
//! rejection keeps its charge (see the parent module's late-CAS test).
use super::{
    bump_metadata_revision, cleanup_options, local_broker, metadata_key, seed_stale_session,
    seed_stale_set, stream_exists, SessionMetadataStore,
};
use anyhow::Result;
use futures_util::FutureExt;
use harnx_runtime::manual_session_cleanup::{
    run_manual_cleanup_with, CleanupTestHook, ManualCleanupOutcome, ManualCleanupOverrides,
    SessionCleanupStatus,
};
use std::sync::Arc;

/// Hook builder: a concurrent writer bumps only the given candidate's
/// canonical revision. The guarded-entry races target one known session,
/// unlike the parent module's every-candidate bump hook.
fn bump_one_candidate_hook(
    store: &SessionMetadataStore,
    storage_key: &str,
) -> Option<CleanupTestHook> {
    let hook_store = store.clone();
    let target = storage_key.to_string();
    Some(Arc::new(move |storage_key: &str| {
        let store = hook_store.clone();
        let target = target.clone();
        let storage_key = storage_key.to_string();
        async move {
            if storage_key == target {
                bump_metadata_revision(&store, &storage_key)
                    .await
                    .expect("bump revision");
            }
        }
        .boxed()
    }))
}

/// Hook builder: a concurrent writer removes the given candidate's canonical
/// metadata record — the removal variant of [`bump_one_candidate_hook`].
fn remove_one_candidate_hook(
    store: &SessionMetadataStore,
    storage_key: &str,
) -> Option<CleanupTestHook> {
    let hook_store = store.clone();
    let target = storage_key.to_string();
    Some(Arc::new(move |storage_key: &str| {
        let store = hook_store.clone();
        let target = target.clone();
        let storage_key = storage_key.to_string();
        async move {
            if storage_key == target {
                store
                    .kv_store()
                    .purge(&metadata_key(&storage_key))
                    .await
                    .expect("purge metadata");
            }
        }
        .boxed()
    }))
}

/// Run the manual pass with one stale day and only the `after_admission`
/// hook set — the single seam every guarded-entry race test drives. The
/// hook overrides go through here rather than repeating the call, so the
/// entry races stay distinct from the late-CAS and dry-run call shapes.
async fn run_with_after_admission(
    config: &harnx_runtime::config::Config,
    max: usize,
    after_admission: Option<CleanupTestHook>,
) -> Result<ManualCleanupOutcome> {
    run_manual_cleanup_with(
        config,
        "local",
        cleanup_options(1, max, false),
        ManualCleanupOverrides {
            after_admission,
            ..ManualCleanupOverrides::default()
        },
    )
    .await
}

/// A canonical update between admission and the guarded cascade entry: the
/// expected-revision check rejects the race, nothing is mutated, and the
/// attempt is refunded — distinct in detail from the later final-CAS
/// rejection, which keeps the charge and reports partial effects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarded_entry_update_after_admission_skips_refunded_without_mutation() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let raced = seed_stale_session(&jetstream, "gc-agent", "entry-update", 1, true).await?;
    let after_admission = bump_one_candidate_hook(&raced.metadata_store, &raced.storage_key);

    let outcome = run_with_after_admission(&config, 1, after_admission).await?;

    assert!(
        !outcome.failed,
        "a read-only entry rejection is a skip, not an error: {outcome:?}"
    );
    let report = &outcome.report;
    assert_eq!(report.scanned, 1);
    assert_eq!(
        report.attempted, 0,
        "the guarded-entry rejection refunded the attempt"
    );
    assert_eq!(report.skipped_revision_changed, 1, "{report:?}");
    assert_eq!(report.deleted, 0);
    assert_eq!(report.errors, 0);
    assert!(
        !report.bound_reached,
        "the refunded attempt admits nothing more only because no candidate remains"
    );
    let result = &report.sessions[0];
    assert_eq!(result.status, SessionCleanupStatus::SkippedRevisionChanged);
    assert!(
        !result.missing_metadata,
        "the record was updated, not removed"
    );
    assert!(
        !result.late,
        "entry rejection, unlike the final-CAS rejection, is not late"
    );
    assert!(!result.partial_possible, "nothing was mutated");

    // No cleanup mutation: the stream, the canonical record and the racing
    // writer's update all survived.
    assert!(stream_exists(&jetstream, &raced.stream_name).await);
    let record = raced
        .metadata_store
        .get(&raced.storage_key)
        .await?
        .expect("canonical record survived the rejected entry");
    assert!(record.revision > raced.revision, "the racing writer won");
    Ok(())
}

/// A canonical removal between admission and the guarded cascade entry: the
/// expected-revision check reports the record missing, nothing is mutated,
/// and the attempt is refunded. `missing` must stay distinct from `changed`
/// in the result the operator reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarded_entry_removal_after_admission_reports_missing_without_mutation() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let raced = seed_stale_session(&jetstream, "gc-agent", "entry-removed", 1, true).await?;
    let after_admission = remove_one_candidate_hook(&raced.metadata_store, &raced.storage_key);

    let outcome = run_with_after_admission(&config, 1, after_admission).await?;

    assert!(
        !outcome.failed,
        "a read-only entry rejection is a skip: {outcome:?}"
    );
    let report = &outcome.report;
    assert_eq!(report.scanned, 1);
    assert_eq!(
        report.attempted, 0,
        "the guarded-entry rejection refunded the attempt"
    );
    assert_eq!(report.skipped_revision_changed, 1, "{report:?}");
    assert_eq!(report.deleted, 0);
    assert_eq!(report.errors, 0);
    let result = &report.sessions[0];
    assert_eq!(result.status, SessionCleanupStatus::SkippedRevisionChanged);
    assert!(
        result.missing_metadata,
        "the removed record is reported missing, not changed"
    );
    assert!(!result.late);
    assert!(!result.partial_possible, "nothing was mutated");

    // No cleanup mutation: the session stream survived the rejected entry.
    // The absent canonical record is the racing writer's removal, not the
    // pass's work.
    assert!(stream_exists(&jetstream, &raced.stream_name).await);
    assert!(raced
        .metadata_store
        .get(&raced.storage_key)
        .await?
        .is_none());
    Ok(())
}

/// The refund is real budget, not cosmetic: under `--max 1` the updated
/// candidate's rejected entry frees the admission, so the later eligible
/// candidate still enters and deletes. The late final-CAS rejection keeps
/// its charge — only the entry rejection, which mutates nothing, refunds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarded_entry_refund_admits_later_eligible_candidate_under_max_one() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let seeded = seed_stale_set(
        &jetstream,
        "gc-agent",
        &[("refund-raced", 1), ("refund-clean", 2)],
        true,
    )
    .await?;
    // Oldest effective activity first: the raced candidate is admitted
    // first, the eligible one right behind it.
    let raced = &seeded[0];
    let clean = &seeded[1];

    let after_admission = bump_one_candidate_hook(&raced.metadata_store, &raced.storage_key);

    let outcome = run_with_after_admission(&config, 1, after_admission).await?;

    assert!(!outcome.failed, "{outcome:?}");
    let report = &outcome.report;
    assert_eq!(report.scanned, 2);
    assert_eq!(
        report.attempted, 1,
        "only the completed cascade charged budget"
    );
    assert_eq!(report.skipped_revision_changed, 1, "{report:?}");
    assert_eq!(
        report.deleted, 1,
        "the refunded admission let the later candidate in"
    );
    assert_eq!(report.errors, 0);

    // The raced candidate is untouched; the later candidate was deleted.
    assert!(stream_exists(&jetstream, &raced.stream_name).await);
    assert!(raced
        .metadata_store
        .get(&raced.storage_key)
        .await?
        .is_some());
    assert!(!stream_exists(&jetstream, &clean.stream_name).await);
    assert!(clean
        .metadata_store
        .get(&clean.storage_key)
        .await?
        .is_none());
    Ok(())
}
