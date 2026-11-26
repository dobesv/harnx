//! Broker-side GC-lease takeover regressions: a competing pass or lease
//! expiry breaks the pass's ownership on the broker (not just the local
//! loss flag) during the final, max-capped, or final previewed candidate.
//! The trailing ownership revalidation after the candidate loop is the only
//! thing that can catch these.
use super::{cleanup_options, local_broker, marker_present, seed_stale_set, stream_exists};
use anyhow::Result;
use futures_util::FutureExt;
use harnx_runtime::manual_session_cleanup::{
    run_manual_cleanup_with, CleanupTestHook, ManualCleanupOverrides,
};
use std::sync::Arc;

/// The GC lease key the manual pass owns, derived exactly like
/// `NatsLeaseConfig::key_for_session` for the `session_metadata_gc` lease.
const GC_LEASE_KEY: &str = "sessions/session_metadata_gc/lock";

/// Steal the GC lease on the broker: a competing pass overwrites the lease
/// record, so the holder's next renewal CAS fails on revision mismatch —
/// the broker-side takeover the local loss watch only sees at a renewal
/// tick. The lease bucket exists by the time any pass hook runs: the pass
/// itself created it acquiring the GC lease.
async fn steal_gc_lease(jetstream: &async_nats::jetstream::Context) -> Result<()> {
    let store = jetstream.get_key_value("harnx_leases").await?;
    store
        .put(
            GC_LEASE_KEY,
            serde_json::to_vec(&serde_json::json!({
                "worker_id": "thief-worker",
                "generation": 1
            }))?
            .into(),
        )
        .await?;
    Ok(())
}

/// Purge the GC lease key on the broker, as a lease-expiry cleanup would.
async fn purge_gc_lease(jetstream: &async_nats::jetstream::Context) -> Result<()> {
    let store = jetstream.get_key_value("harnx_leases").await?;
    store.purge(GC_LEASE_KEY).await?;
    Ok(())
}

/// How a sabotage hook breaks the pass's GC lease on the broker.
#[derive(Clone, Copy)]
enum LeaseSabotage {
    /// A competing pass overwrites the lease record.
    Steal,
    /// A lease-expiry cleanup purges the lease key.
    Purge,
}

/// An after-entry hook that sabotages the pass's GC lease on the broker at
/// the injected point, so the takeover races a live cascade exactly like a
/// real competing pass would.
fn gc_lease_sabotage_hook(
    jetstream: &async_nats::jetstream::Context,
    sabotage: LeaseSabotage,
) -> Option<CleanupTestHook> {
    let jetstream = jetstream.clone();
    Some(Arc::new(move |_storage_key: &str| {
        let jetstream = jetstream.clone();
        async move {
            let sabotaged = match sabotage {
                LeaseSabotage::Steal => steal_gc_lease(&jetstream).await,
                LeaseSabotage::Purge => purge_gc_lease(&jetstream).await,
            };
            sabotaged.expect("sabotage GC lease");
        }
        .boxed()
    }))
}

/// A broker-side takeover during the final cascade must still report the
/// lease loss (and a nonzero exit) even though the loop has no next
/// iteration left to revalidate on. The cascade itself completes under its
/// own guards, so its deletion still counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_cascade_broker_lease_steal_reports_failure_after_deletion() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let seeded = seed_stale_set(&jetstream, "gc-agent", &[("steal-final", 1)], true).await?;
    let after_entry = gc_lease_sabotage_hook(&jetstream, LeaseSabotage::Steal);

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, false),
        ManualCleanupOverrides {
            after_entry,
            ..ManualCleanupOverrides::default()
        },
    )
    .await?;

    assert!(
        outcome.failed,
        "final-candidate broker loss must fail the CLI: {outcome:?}"
    );
    let report = &outcome.report;
    assert!(report.lease_lost, "{report:?}");
    assert_eq!(
        report.deleted, 1,
        "the cascade completed under its own guards"
    );
    assert!(!stream_exists(&jetstream, &seeded[0].stream_name).await);
    Ok(())
}

/// The same takeover on the max-capped final candidate: the bound check
/// breaks the loop before the loop's own revalidation would run, so only
/// the post-loop broker revalidation can catch it. The capped-out candidate
/// stays untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_capped_final_cascade_broker_lease_purge_reports_failure() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let seeded = seed_stale_set(
        &jetstream,
        "gc-agent",
        &[("purge-capped", 1), ("purge-capped-2", 2)],
        true,
    )
    .await?;
    let after_entry = gc_lease_sabotage_hook(&jetstream, LeaseSabotage::Purge);

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 1, false),
        ManualCleanupOverrides {
            after_entry,
            ..ManualCleanupOverrides::default()
        },
    )
    .await?;

    assert!(outcome.failed, "capped-candidate broker loss: {outcome:?}");
    let report = &outcome.report;
    assert!(report.lease_lost, "{report:?}");
    assert!(report.bound_reached);
    assert_eq!(report.deleted, 1);
    // The capped-out candidate was never admitted and is fully intact.
    assert!(stream_exists(&jetstream, &seeded[1].stream_name).await);
    assert!(seeded[1]
        .metadata_store
        .get(&seeded[1].storage_key)
        .await?
        .is_some());
    Ok(())
}

/// A broker-side GC-lease purge during the LAST previewed candidate: no
/// session-owned write happened, but the pass still reports the loss so the
/// operator knows the preview ran without coordination at the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preview_broker_lease_purge_reports_failure_without_writes() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let seeded = seed_stale_set(
        &jetstream,
        "gc-agent",
        &[("preview-loss", 1), ("preview-loss-2", 2)],
        true,
    )
    .await?;
    let hook_jetstream = jetstream.clone();
    let last_key = seeded[1].storage_key.clone();

    let before_candidate: Option<CleanupTestHook> = Some(Arc::new(move |storage_key: &str| {
        let jetstream = hook_jetstream.clone();
        let is_last = storage_key == last_key;
        async move {
            if is_last {
                purge_gc_lease(&jetstream).await.expect("purge GC lease");
            }
        }
        .boxed()
    }));

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, true),
        ManualCleanupOverrides {
            before_candidate,
            ..ManualCleanupOverrides::default()
        },
    )
    .await?;

    assert!(outcome.failed, "preview broker loss: {outcome:?}");
    let report = &outcome.report;
    assert!(report.lease_lost, "{report:?}");
    assert_eq!(
        report.would_delete, 2,
        "both previews were computed: {report:?}"
    );
    assert_eq!(report.deleted, 0);
    // The preview stayed a preview even across the loss.
    for record in &seeded {
        assert!(stream_exists(&jetstream, &record.stream_name).await);
        assert!(record
            .metadata_store
            .get(&record.storage_key)
            .await?
            .is_some());
    }
    assert!(!marker_present(&config).await?);
    Ok(())
}
