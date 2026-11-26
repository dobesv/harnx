//! Broker-spawned tests for the manual expired-session cleanup pass
//! (`harnx delete expired-sessions`, #2923 T2): bounded admissions, preview
//! no-write guarantees, revision races, GC-lease ownership, and the shared
//! A2A leader-read hold matrix. All races are driven by deterministic hooks,
//! never sleeps.
use crate::common::spawn_nats_server;

mod guarded_entry;
mod lease_loss;

use anyhow::{Context as _, Result};
use async_nats::jetstream::stream;
use chrono::{TimeZone, Utc};
use futures_util::FutureExt;
use harnx_core::require_nextest;
use harnx_runtime::config::{Config, NatsServerConfig};
use harnx_runtime::manual_session_cleanup::{
    run_manual_cleanup_with, CleanupTestHook, ManualCleanupOptions, ManualCleanupOverrides,
    SessionCleanupStatus,
};
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use harnx_runtime::nats_session_log::stream_name_for_session;
use harnx_runtime::nats_session_metadata::{
    activity_key, metadata_key, SessionActivity, SessionAgentSource, SessionInitializer,
    SessionMetadata, SessionMetadataStore,
};
use harnx_runtime::remote_session_cleanup::{acquire_session_gc_lease, GC_LAST_RUN_EPOCH_HOUR_KEY};
use std::sync::Arc;

/// A seeded stale session: metadata, a stale activity record and (optionally)
/// a stream with one message.
struct Seeded {
    storage_key: String,
    stream_name: String,
    metadata_store: SessionMetadataStore,
    revision: u64,
}

/// Create the metadata record (returning its revision) and the stale
/// activity entry, then optionally a session stream with one message —
/// the fixture shape every stale-session test shares.
async fn seed_session_record(
    jetstream: &async_nats::jetstream::Context,
    metadata: SessionMetadata,
    activity_secs: i64,
    with_stream: bool,
) -> Result<Seeded> {
    let metadata_store = SessionMetadataStore::ensure(jetstream, 1).await?;
    let storage_key = metadata.storage_key();
    let revision = metadata_store
        .create(&metadata)
        .await?
        .with_context(|| format!("seed metadata for '{storage_key}'"))?;
    metadata_store
        .kv_store()
        .put(
            activity_key(&storage_key),
            serde_json::to_vec(&SessionActivity {
                first_activation_at: None,
                last_activity_at: Utc.timestamp_opt(activity_secs, 0).single().expect("time"),
            })?
            .into(),
        )
        .await?;
    let stream_name = stream_name_for_session(&storage_key);
    if with_stream {
        jetstream
            .create_stream(stream::Config {
                name: stream_name.clone(),
                subjects: vec![format!("sessions.{storage_key}.>")],
                storage: stream::StorageType::File,
                ..Default::default()
            })
            .await?;
        jetstream
            .publish(format!("sessions.{storage_key}.event"), "stale".into())
            .await?
            .await?;
    }
    Ok(Seeded {
        storage_key,
        stream_name,
        metadata_store,
        revision,
    })
}

/// A seeded stale named-agent session: metadata, a stale activity record
/// and (optionally) a stream with one message.
async fn seed_stale_session(
    jetstream: &async_nats::jetstream::Context,
    agent: &str,
    session_id: &str,
    activity_secs: i64,
    with_stream: bool,
) -> Result<Seeded> {
    let metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::named(agent, Default::default()),
    );
    seed_session_record(jetstream, metadata, activity_secs, with_stream).await
}

/// A seeded stale inline-agent session: metadata only, never selected by a
/// named-agent filter.
async fn seed_inline_stale_session(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    activity_secs: i64,
) -> Result<Seeded> {
    let mut metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::named("unused", Default::default()),
    );
    metadata.agent = SessionAgentSource::Inline {
        instructions: "inline test agent".to_string(),
    };
    seed_session_record(jetstream, metadata, activity_secs, false).await
}

/// Seed several stale sessions for one agent; the result order matches the
/// argument order so tests can index deterministically.
async fn seed_stale_set(
    jetstream: &async_nats::jetstream::Context,
    agent: &str,
    sessions: &[(&str, i64)],
    with_stream: bool,
) -> Result<Vec<Seeded>> {
    let mut seeded = Vec::new();
    for (session_id, activity_secs) in sessions {
        seeded.push(
            seed_stale_session(jetstream, agent, session_id, *activity_secs, with_stream).await?,
        );
    }
    Ok(seeded)
}

/// Hook builder: a concurrent writer bumps the candidate's canonical
/// revision whenever the hook fires, whichever session it targets.
fn bump_revision_hook(store: &SessionMetadataStore) -> Option<CleanupTestHook> {
    let hook_store = store.clone();
    Some(Arc::new(move |storage_key: &str| {
        let store = hook_store.clone();
        let storage_key = storage_key.to_string();
        async move {
            bump_metadata_revision(&store, &storage_key)
                .await
                .expect("bump revision");
        }
        .boxed()
    }))
}

/// Hook builder: an unresolved A2A context appears on the candidate
/// whenever the hook fires — the read-only predicate has cleared, the live
/// fence refuses.
fn unresolved_context_hook(store: &SessionMetadataStore) -> Option<CleanupTestHook> {
    let hook_store = store.clone();
    Some(Arc::new(move |storage_key: &str| {
        let store = hook_store.clone();
        let storage_key = storage_key.to_string();
        async move {
            store
                .kv_store()
                .put(
                    format!("sessions/{storage_key}/a2a/context"),
                    serde_json::to_vec(&serde_json::json!({
                        "state": {"active": {"stop_confirmed": false}}
                    }))
                    .expect("context")
                    .into(),
                )
                .await
                .expect("write context");
        }
        .boxed()
    }))
}

/// The shared broker prologue: require nextest isolation, spawn the
/// ephemeral server, connect. `None` means no nats-server binary; the
/// caller skips like it always did.
async fn local_broker() -> Result<
    Option<(
        crate::common::NatsServerHandle,
        Config,
        async_nats::jetstream::Context,
    )>,
> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(None);
    };
    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    // The returned handle keeps the spawned broker alive for the whole
    // test; dropping it kills the process.
    Ok(Some((server, config, jetstream)))
}

fn local_nats_config(url: &str) -> Config {
    Config {
        nats_servers: vec![NatsServerConfig {
            user_id: None,
            name: "local".to_string(),
            url: url.to_string(),
            token: None,
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            ignore_discovered_servers: None,
            agents: vec![],
        }],
        ..Default::default()
    }
}

fn cleanup_options(days: u64, max: usize, dry_run: bool) -> ManualCleanupOptions {
    ManualCleanupOptions {
        days,
        agent: None,
        max,
        dry_run,
    }
}

async fn stream_exists(jetstream: &async_nats::jetstream::Context, name: &str) -> bool {
    jetstream.get_stream(name).await.is_ok()
}

async fn marker_present(config: &Config) -> Result<bool> {
    let leases = config.nats_kv_bucket("local", "harnx_leases").await?;
    Ok(leases.get(GC_LAST_RUN_EPOCH_HOUR_KEY).await?.is_some())
}

async fn hold_session_lease(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<NatsSessionLease> {
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: jetstream.clone(),
        session_id: storage_key,
        worker_id: "lease-holder".to_string(),
        generation: 1,
        config: NatsLeaseConfig::default(),
        session_metadata: None,
    })
    .await?
    .context("acquire session lease")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deletes_stale_keeps_fresh_and_leases_and_leaves_orphans_and_marker_alone() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let stale = seed_stale_session(&jetstream, "gc-agent", "manual-stale", 1, true).await?;
    let fresh = seed_stale_session(
        &jetstream,
        "gc-agent",
        "manual-fresh",
        100_000_000_000,
        true,
    )
    .await
    .unwrap_or_else(|_| panic!("seed fresh"));
    let leased = seed_stale_session(&jetstream, "gc-agent", "manual-leased", 1, true).await?;
    let inline = seed_inline_stale_session(&jetstream, "manual-inline", 1).await?;
    // An orphan stream with no metadata: the manual pass must not touch it.
    let orphan_key = harnx_core::session_identity::session_key(Some("gc-agent"), "manual-orphan");
    let orphan_stream = stream_name_for_session(&orphan_key);
    jetstream
        .create_stream(stream::Config {
            name: orphan_stream.clone(),
            subjects: vec![format!("sessions.{orphan_key}.>")],
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await?;
    let lease = hold_session_lease(&jetstream, &leased.storage_key).await?;

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, false),
        ManualCleanupOverrides::default(),
    )
    .await?;

    assert!(!outcome.failed, "clean pass: {outcome:?}");
    let report = &outcome.report;
    assert_eq!(report.scanned, 3, "fresh is not a candidate: {report:?}");
    assert_eq!(report.deleted, 2, "stale + inline: {report:?}");
    assert_eq!(report.skipped_active, 1, "leased: {report:?}");
    assert_eq!(report.attempted, 2);
    assert_eq!(report.errors, 0);
    assert!(!report.bound_reached);
    assert!(!report.lease_lost);
    assert!(report.stream_bytes_deleted > 0, "stream bytes counted");

    assert!(!stream_exists(&jetstream, &stale.stream_name).await);
    assert!(stale
        .metadata_store
        .get(&stale.storage_key)
        .await?
        .is_none());
    assert!(stream_exists(&jetstream, &fresh.stream_name).await);
    assert!(fresh
        .metadata_store
        .get(&fresh.storage_key)
        .await?
        .is_some());
    assert!(stream_exists(&jetstream, &leased.stream_name).await);
    assert!(leased
        .metadata_store
        .get(&leased.storage_key)
        .await?
        .is_some());
    assert!(inline
        .metadata_store
        .get(&inline.storage_key)
        .await?
        .is_none());
    // No orphan pass and no epoch-hour marker: worker behavior stays alone.
    assert!(stream_exists(&jetstream, &orphan_stream).await);
    assert!(!marker_present(&config).await?);
    lease.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_filter_scopes_to_named_agent_and_skips_inline() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let mine = seed_stale_session(&jetstream, "athena", "filter-mine", 1, true).await?;
    let other = seed_stale_session(&jetstream, "ares", "filter-other", 1, true).await?;
    let inline = seed_inline_stale_session(&jetstream, "filter-inline", 1).await?;

    let options = ManualCleanupOptions {
        agent: Some("athena".to_string()),
        ..cleanup_options(1, 100, false)
    };
    let outcome =
        run_manual_cleanup_with(&config, "local", options, ManualCleanupOverrides::default())
            .await?;

    assert!(!outcome.failed);
    let report = &outcome.report;
    assert_eq!(report.scanned, 1);
    assert_eq!(report.agent_filter.as_deref(), Some("athena"));
    assert_eq!(report.deleted, 1);
    assert!(!stream_exists(&jetstream, &mine.stream_name).await);
    assert!(other
        .metadata_store
        .get(&other.storage_key)
        .await?
        .is_some());
    assert!(stream_exists(&jetstream, &other.stream_name).await);
    // A named filter never selects inline sessions.
    assert!(inline
        .metadata_store
        .get(&inline.storage_key)
        .await?
        .is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dry_run_previews_bounded_selections_without_writes() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let seeded = seed_stale_set(
        &jetstream,
        "gc-agent",
        &[("dry-older", 1), ("dry-newer", 2)],
        true,
    )
    .await?;
    let (older, newer) = (&seeded[0], &seeded[1]);

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 1, true),
        ManualCleanupOverrides::default(),
    )
    .await?;

    assert!(!outcome.failed);
    let report = &outcome.report;
    assert_eq!(
        report.mode,
        harnx_runtime::manual_session_cleanup::ManualCleanupMode::DryRun
    );
    assert_eq!(report.scanned, 2);
    assert_eq!(report.would_delete, 1, "preview bound applies: {report:?}");
    assert_eq!(report.attempted, 0);
    assert_eq!(report.deleted, 0);
    assert!(report.bound_reached, "cap stopped the preview: {report:?}");
    assert_eq!(report.sessions.len(), 1);
    let preview = &report.sessions[0];
    assert_eq!(preview.storage_key, older.storage_key, "oldest first");
    assert!(preview.stream_bytes > 0);
    assert_eq!(preview.status, SessionCleanupStatus::WouldDelete);

    // No session-owned mutation: everything the pass observed is intact.
    for seeded in [older, newer] {
        assert!(stream_exists(&jetstream, &seeded.stream_name).await);
        let record = seeded
            .metadata_store
            .get(&seeded.storage_key)
            .await?
            .expect("metadata intact");
        assert_eq!(record.revision, seeded.revision, "revision untouched");
        let activity = seeded
            .metadata_store
            .get_activity(&seeded.storage_key)
            .await?
            .expect("activity intact");
        assert!(activity.last_activity_at < Utc::now() - chrono::Duration::hours(1));
    }
    assert!(!marker_present(&config).await?);
    Ok(())
}

/// Seed a plan document the cascade's plans cleanup rejects (invalid UTF-8),
/// mirroring the existing session-delete partial-failure fixture: the stream
/// and journal are removed, the owner record survives.
async fn seed_poisoned_plan(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<()> {
    let plans = harnx_blob_store::plans::ensure_plans_bucket(jetstream, 1).await?;
    let plan_key = format!("plan/{storage_key}/test/index.md");
    plans.put(&plan_key, vec![0xff].into()).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_bounds_admissions_including_late_cascade_failures() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let poisoned = seed_stale_session(&jetstream, "gc-agent", "bound-poisoned", 1, true).await?;
    seed_poisoned_plan(&jetstream, &poisoned.storage_key).await?;
    let clean_older = seed_stale_session(&jetstream, "gc-agent", "bound-clean", 2, true).await?;
    let clean_newest = seed_stale_session(&jetstream, "gc-agent", "bound-untouched", 3, true)
        .await
        .unwrap_or_else(|_| panic!("seed newest"));

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 2, false),
        ManualCleanupOverrides::default(),
    )
    .await?;

    // The poisoned cascade failed after deleting the stream: partial effects,
    // charged attempt, nonzero exit.
    assert!(
        outcome.failed,
        "partial effects must fail the CLI: {outcome:?}"
    );
    let report = &outcome.report;
    assert_eq!(report.scanned, 3);
    assert_eq!(report.attempted, 2, "failure consumed budget: {report:?}");
    assert_eq!(report.deleted, 1);
    assert_eq!(report.errors, 1);
    assert!(report.bound_reached, "cap stopped the pass: {report:?}");
    assert_eq!(report.sessions.len(), 2);

    let poisoned_result = &report.sessions[0];
    assert_eq!(poisoned_result.status, SessionCleanupStatus::Failed);
    assert!(poisoned_result.partial_possible);
    assert!(poisoned_result.detail.is_some(), "failure detail retained");

    // Honest partial effects: the cascade removed the stream but the owner
    // record survived for a retry.
    assert!(!stream_exists(&jetstream, &poisoned.stream_name).await);
    assert!(poisoned
        .metadata_store
        .get(&poisoned.storage_key)
        .await?
        .is_some());

    assert!(!stream_exists(&jetstream, &clean_older.stream_name).await);
    assert!(clean_older
        .metadata_store
        .get(&clean_older.storage_key)
        .await?
        .is_none());
    // The bound stopped the third candidate before touching it.
    assert!(stream_exists(&jetstream, &clean_newest.stream_name).await);
    assert!(clean_newest
        .metadata_store
        .get(&clean_newest.storage_key)
        .await?
        .is_some());
    Ok(())
}

/// Bump the canonical metadata record's revision without changing its
/// payload, deterministically simulating a concurrent writer.
async fn bump_metadata_revision(
    metadata_store: &SessionMetadataStore,
    storage_key: &str,
) -> Result<()> {
    let key = metadata_key(storage_key);
    let record = metadata_store
        .kv_store()
        .get(&key)
        .await?
        .with_context(|| format!("metadata present for '{storage_key}'"))?;
    metadata_store.kv_store().put(&key, record).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_entry_revision_and_activity_races_skip_without_charging_budget() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let seeded = seed_stale_set(
        &jetstream,
        "gc-agent",
        &[("race-changed", 1), ("race-missing", 2), ("race-fresh", 3)],
        true,
    )
    .await?;
    let (changed, missing, reactivated) = (&seeded[0], &seeded[1], &seeded[2]);

    let hook_store = changed.metadata_store.clone();
    let changed_key = changed.storage_key.clone();
    let missing_key = missing.storage_key.clone();
    let fresh_key = reactivated.storage_key.clone();
    let before_candidate: Option<CleanupTestHook> = Some(Arc::new(move |storage_key: &str| {
        let store = hook_store.clone();
        let storage_key = storage_key.to_string();
        let changed_key = changed_key.clone();
        let missing_key = missing_key.clone();
        let fresh_key = fresh_key.clone();
        async move {
            if storage_key == changed_key {
                bump_metadata_revision(&store, &storage_key)
                    .await
                    .expect("bump revision");
            } else if storage_key == missing_key {
                store
                    .kv_store()
                    .purge(&metadata_key(&storage_key))
                    .await
                    .expect("purge metadata");
            } else if storage_key == fresh_key {
                store
                    .kv_store()
                    .put(
                        activity_key(&storage_key),
                        serde_json::to_vec(&SessionActivity {
                            first_activation_at: None,
                            last_activity_at: Utc::now(),
                        })
                        .expect("activity")
                        .into(),
                    )
                    .await
                    .expect("refresh activity");
            }
        }
        .boxed()
    }));

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, false),
        ManualCleanupOverrides {
            before_candidate,
            ..ManualCleanupOverrides::default()
        },
    )
    .await?;

    assert!(
        !outcome.failed,
        "known races are skips, not errors: {outcome:?}"
    );
    let report = &outcome.report;
    assert_eq!(report.scanned, 3);
    assert_eq!(report.skipped_revision_changed, 2, "{report:?}");
    assert_eq!(report.skipped_active, 1);
    assert_eq!(report.attempted, 0, "read-only races charge no budget");
    assert_eq!(report.deleted, 0);
    assert_eq!(report.errors, 0);

    let missing_result = report
        .sessions
        .iter()
        .find(|result| result.storage_key == missing.storage_key)
        .expect("missing result");
    assert!(
        missing_result.missing_metadata,
        "missing distinct from changed"
    );

    // Nothing was deleted through a race.
    for seeded in [changed, missing, reactivated] {
        assert!(stream_exists(&jetstream, &seeded.stream_name).await);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_cas_rejection_reports_partial_effects_and_charges_attempt() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let raced = seed_stale_session(&jetstream, "gc-agent", "late-cas", 1, true).await?;
    let after_entry = bump_revision_hook(&raced.metadata_store);

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
        "possible partial effects fail the CLI: {outcome:?}"
    );
    let report = &outcome.report;
    assert_eq!(report.scanned, 1);
    assert_eq!(report.attempted, 1, "the late rejection charged budget");
    assert_eq!(report.deleted, 0);
    assert_eq!(report.skipped_revision_changed, 1, "{report:?}");
    assert_eq!(report.errors, 0);
    let result = &report.sessions[0];
    assert!(result.late, "late detail set: {result:?}");
    assert!(result.partial_possible, "partial detail set: {result:?}");

    // Honest reporting, not a no-op claim: the cascade removed the stream
    // and owned resources before the final CAS rejected the bumped revision.
    assert!(!stream_exists(&jetstream, &raced.stream_name).await);
    let record = raced
        .metadata_store
        .get(&raced.storage_key)
        .await?
        .expect("canonical record survived the rejected purge");
    assert!(record.revision > raced.revision, "the racing writer won");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_fence_refusal_is_charged_skipped_active() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let refused = seed_stale_session(&jetstream, "gc-agent", "fence-race", 1, true).await?;
    let after_admission = unresolved_context_hook(&refused.metadata_store);

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, false),
        ManualCleanupOverrides {
            after_admission,
            ..ManualCleanupOverrides::default()
        },
    )
    .await?;

    // A fence refusal is a late active skip, not an error; it still consumed
    // its attempt budget.
    assert!(!outcome.failed, "{outcome:?}");
    let report = &outcome.report;
    assert_eq!(report.skipped_active, 1);
    assert_eq!(report.attempted, 1, "the refusal charged budget");
    assert_eq!(report.deleted, 0);
    assert_eq!(report.errors, 0);
    let result = &report.sessions[0];
    assert!(result.late);
    assert!(!result.partial_possible, "nothing was mutated");
    assert!(stream_exists(&jetstream, &refused.stream_name).await);
    assert!(refused
        .metadata_store
        .get(&refused.storage_key)
        .await?
        .is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_lease_loss_after_completed_candidate_stops_admissions() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let seeded = seed_stale_set(
        &jetstream,
        "gc-agent",
        &[("loss-first", 1), ("loss-second", 2)],
        true,
    )
    .await?;
    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, false),
        ManualCleanupOverrides {
            simulate_lease_loss_after: Some(1),
            ..ManualCleanupOverrides::default()
        },
    )
    .await?;

    // The first cascade completed under its own guards; the pass then
    // reported the loss and admitted nothing new.
    assert!(outcome.failed, "lease loss fails the CLI: {outcome:?}");
    let report = &outcome.report;
    assert!(report.lease_lost);
    assert!(!report.bound_reached);
    assert_eq!(report.deleted, 1);
    assert_eq!(report.attempted, 1);
    let (first, second) = (&seeded[0], &seeded[1]);
    assert!(!stream_exists(&jetstream, &first.stream_name).await);
    assert!(stream_exists(&jetstream, &second.stream_name).await);
    assert!(second
        .metadata_store
        .get(&second.storage_key)
        .await?
        .is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_lease_contention_fails_clearly() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let _ = seed_stale_session(&jetstream, "gc-agent", "contention-stale", 1, true).await?;

    let holder = acquire_session_gc_lease(&config, "local")
        .await?
        .expect("test holds the GC lease");
    let error = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, false),
        ManualCleanupOverrides::default(),
    )
    .await
    .expect_err("contention must fail, not wait");
    assert!(
        error.to_string().contains("GC lease"),
        "clear contention error: {error:#}"
    );
    holder.release().await?;

    // With the lease released the same pass succeeds.
    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, false),
        ManualCleanupOverrides::default(),
    )
    .await?;
    assert!(!outcome.failed);
    assert_eq!(outcome.report.deleted, 1);
    Ok(())
}

/// The shared A2A hold matrix, decided from leader-read preview facts through
/// the same predicate the live fence uses. Dry-run keeps every A2A fixture
/// unmutated, so the whole matrix runs in one pass.
/// Seed one matrix case's A2A facts on the shared KV store. Cases cover the
/// full leader-read fact space the live fence decides from: absent state,
/// an unresolved active task, marker contexts (deleted and purged), an
/// unsettled first-message reservation, a settled one, a retired active
/// task, and malformed context JSON.
async fn seed_matrix_fact(kv: &SessionMetadataStore, storage_key: &str, case: &str) -> Result<()> {
    let unresolved = serde_json::json!({"state": {"active": {"stop_confirmed": false}}});
    let retired = serde_json::json!({
        "state": {"active": {
            "snapshot": {"task": {"status": {"state": "TASK_STATE_COMPLETED"}}},
            "stop_confirmed": true,
            "projections": {"archive": true, "message_mapping": true, "final_event": true},
            "publication": {"pending": null}
        }}
    });
    let identity = serde_json::json!({"user": case});
    let registry = serde_json::json!({"first": {"identity": identity}});
    let reservation = serde_json::json!({
        "allocation": {"storage_key": storage_key, "task_id": format!("local.{case}-uuid")}
    });
    let reservation_key = reservation_key_from_identity(&identity);
    match case {
        "matrix-absent" => {}
        "matrix-unresolved" => {
            kv.kv_store()
                .put(context_key(storage_key), unresolved.to_string().into())
                .await?;
        }
        "matrix-ctx-delete" | "matrix-ctx-purge" => {
            // Marker contexts skip the first-reservation rule, like the
            // live fence; the reservation cannot hold them.
            kv.kv_store()
                .put(context_key(storage_key), "seeded".into())
                .await?;
            kv.kv_store()
                .put(registry_key(storage_key), registry.to_string().into())
                .await?;
            kv.kv_store()
                .put(reservation_key, reservation.to_string().into())
                .await?;
            if case == "matrix-ctx-delete" {
                kv.kv_store().delete(context_key(storage_key)).await?;
            } else {
                kv.kv_store().purge(context_key(storage_key)).await?;
            }
        }
        "matrix-unsettled" | "matrix-settled" | "matrix-retired" => {
            kv.kv_store()
                .put(registry_key(storage_key), registry.to_string().into())
                .await?;
            kv.kv_store()
                .put(reservation_key, reservation.to_string().into())
                .await?;
            if case == "matrix-retired" {
                kv.kv_store()
                    .put(context_key(storage_key), retired.to_string().into())
                    .await?;
            }
            if case == "matrix-settled" {
                kv.kv_store()
                    .put(
                        archive_key(storage_key, case),
                        r#"{"kind":"archive"}"#.into(),
                    )
                    .await?;
            }
        }
        "matrix-malformed" => {
            kv.kv_store()
                .put(context_key(storage_key), "not json".into())
                .await?;
        }
        _ => unreachable!("unknown matrix case: {case}"),
    }
    Ok(())
}

/// The shared A2A hold matrix, decided from leader-read preview facts through
/// the same predicate the live fence uses. Dry-run keeps every A2A fixture
/// unmutated, so the whole matrix runs in one pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2a_leader_read_matrix_decides_holds_from_leader_facts() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let cases = [
        ("matrix-absent", SessionCleanupStatus::WouldDelete),
        ("matrix-unresolved", SessionCleanupStatus::SkippedActive),
        // Marker contexts hold no unresolved state and skip the reservation
        // rule; the reservations seeded under them cannot hold.
        ("matrix-ctx-delete", SessionCleanupStatus::WouldDelete),
        ("matrix-ctx-purge", SessionCleanupStatus::WouldDelete),
        ("matrix-unsettled", SessionCleanupStatus::SkippedActive),
        ("matrix-settled", SessionCleanupStatus::WouldDelete),
        // A non-null retired active state satisfies the reservation rule and
        // is not unresolved.
        ("matrix-retired", SessionCleanupStatus::WouldDelete),
        ("matrix-malformed", SessionCleanupStatus::Error),
    ];
    let mut seeded = Vec::new();
    for (index, (case, _)) in cases.iter().enumerate() {
        seeded
            .push(seed_stale_session(&jetstream, "gc-agent", case, 1 + index as i64, false).await?);
    }
    for ((case, _), record) in cases.iter().zip(&seeded) {
        seed_matrix_fact(&record.metadata_store, &record.storage_key, case).await?;
    }

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, true),
        ManualCleanupOverrides::default(),
    )
    .await?;

    // Malformed A2A state the pass must inspect is a per-session error, so
    // the CLI would exit nonzero even though nothing was deleted.
    assert!(outcome.failed, "{outcome:?}");
    let report = &outcome.report;
    assert_eq!(report.scanned, 8);
    assert_eq!(report.attempted, 0, "dry-run never admits to a cascade");
    assert_eq!(report.deleted, 0);
    assert_eq!(report.would_delete, 5, "{report:?}");
    assert_eq!(
        report.skipped_active, 2,
        "unresolved + unsettled: {report:?}"
    );
    assert_eq!(report.errors, 1, "malformed context: {report:?}");

    let status_of = |case: &str| -> SessionCleanupStatus {
        let storage_key = harnx_core::session_identity::session_key(Some("gc-agent"), case);
        report
            .sessions
            .iter()
            .find(|result| result.storage_key == storage_key)
            .unwrap_or_else(|| panic!("result for {case}: {report:?}"))
            .status
    };
    for (case, expected) in cases {
        assert_eq!(status_of(case), expected, "{case}: {report:?}");
    }

    // Dry-run leaves every A2A fact it observed unmutated.
    let unresolved_key = context_key(&harnx_core::session_identity::session_key(
        Some("gc-agent"),
        "matrix-unresolved",
    ));
    let kv = &seeded[0].metadata_store;
    let entry = kv.kv_store().entry(unresolved_key).await?.expect("context");
    assert!(matches!(
        entry.operation,
        async_nats::jetstream::kv::Operation::Put
    ));
    Ok(())
}

/// A stream-probe failure on the second candidate must not abort the pass or
/// discard the first candidate's completed deletion: it is a per-session
/// error, charged as no admission, and the pass finishes so the summary
/// keeps both results.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_probe_error_after_completed_deletion_preserves_summary() -> Result<()> {
    let Some((_server, config, jetstream)) = local_broker().await? else {
        return Ok(());
    };
    let seeded = seed_stale_set(
        &jetstream,
        "gc-agent",
        &[("probe-ok", 1), ("probe-fail", 2)],
        true,
    )
    .await?;

    let outcome = run_manual_cleanup_with(
        &config,
        "local",
        cleanup_options(1, 100, false),
        ManualCleanupOverrides {
            fail_stream_probe_for: Some(seeded[1].storage_key.clone()),
            ..ManualCleanupOverrides::default()
        },
    )
    .await?;

    assert!(
        outcome.failed,
        "the probe error must fail the CLI: {outcome:?}"
    );
    let report = &outcome.report;
    assert_eq!(
        report.deleted, 1,
        "earlier deletion survives in the summary"
    );
    assert_eq!(report.attempted, 1, "a probe error charges no admission");
    assert_eq!(report.errors, 1);
    assert!(!report.lease_lost);
    assert!(!report.bound_reached);
    assert_eq!(report.sessions.len(), 2);
    let probe_failed = &report.sessions[1];
    assert_eq!(probe_failed.status, SessionCleanupStatus::Error);
    assert!(probe_failed.detail.is_some());
    assert!(!probe_failed.partial_possible, "nothing was mutated");
    // The failed candidate is fully intact for a retry.
    assert!(!stream_exists(&jetstream, &seeded[0].stream_name).await);
    assert!(stream_exists(&jetstream, &seeded[1].stream_name).await);
    assert!(seeded[1]
        .metadata_store
        .get(&seeded[1].storage_key)
        .await?
        .is_some());
    Ok(())
}

/// The A2A key layout the fence and the shared predicate both use; the same
/// derivation as `nats_admin::a2a::hold`'s helpers, which are crate-private.
fn context_key(storage_key: &str) -> String {
    format!("sessions/{storage_key}/a2a/context")
}

fn registry_key(storage_key: &str) -> String {
    format!("a2a.registry.{}", harnx_core::crypto::sha256(storage_key))
}

fn reservation_key_from_identity(identity: &serde_json::Value) -> String {
    // The registry's first identity derives the reservation key; the shared
    // predicate hashes the identity's JSON the same way.
    format!(
        "a2a/first-messages/{}",
        harnx_core::crypto::sha256(&identity.to_string())
    )
}

fn archive_key(storage_key: &str, case: &str) -> String {
    // The reservation task id `local.<case>-uuid` derives the archive key the
    // way the shared predicate does: everything after the last dot.
    format!("sessions/{storage_key}/a2a/archive/{case}-uuid")
}
