//! Real-broker tests for the bulk collectors. These spawn a `nats-server`, so
//! their test names contain `nats` to land in the `broker-e2e` groups the
//! `.config/nextest.toml` filters select in both the default and `ci` profiles
//! (verify with `cargo nextest show-config test-groups`).
use super::aggregate;
use super::report::{ClusterStatsReport, SessionStatsDetail, SharedBucketReport, StatsAgentSource};
use super::snapshot;
use crate::nats_lease::{ensure_lease_bucket, NatsLeaseConfig};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{
    activity_key, metadata_key, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use crate::nats_test_common::spawn_nats_server;
use anyhow::{ensure, Context as _, Result};
use async_nats::jetstream;
use async_nats::jetstream::kv;
use bytes::Bytes;
use chrono::{Duration, Utc};
use futures_util::StreamExt;
use harnx_core::agent_config::AgentVariables;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use std::time::Duration as StdDuration;

/// One named session's canonical metadata, backdated activity and a session
/// stream with one user event. Returns the storage key.
async fn seed_session(
    js: &jetstream::Context,
    store: &SessionMetadataStore,
    agent: &str,
    session_id: &str,
    with_stream: bool,
) -> Result<String> {
    let metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::named(agent, AgentVariables::default()),
    );
    store.create(&metadata).await?;
    let storage_key = metadata.storage_key();
    store.touch_activity(&storage_key).await?;
    if with_stream {
        let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key.clone(), 1);
        log.append_event_async(&user_event("seeded event")).await?;
    }
    Ok(storage_key)
}

fn user_event(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

/// Overwrite the activity record with a backdated timestamp, the way a session
/// that has been idle for `days` looks to cleanup.
async fn backdate_activity(
    store: &SessionMetadataStore,
    storage_key: &str,
    days: i64,
) -> Result<()> {
    let activity = serde_json::json!({
        "last_activity_at": (Utc::now() - Duration::days(days)).to_rfc3339(),
    });
    store
        .kv_store()
        .put(
            activity_key(storage_key),
            serde_json::to_vec(&activity)?.into(),
        )
        .await
        .with_context(|| format!("backdate activity for '{storage_key}'"))?;
    Ok(())
}

async fn put_lease(bucket: &kv::Store, key: &str) -> Result<()> {
    bucket
        .put(key.to_string(), b"lease-record".to_vec().into())
        .await
        .with_context(|| format!("seed lease key '{key}'"))?;
    Ok(())
}

/// Seed `sessions` named sessions, `streamed` of them with a session stream:
/// the first five are idle 40 days (past a 30-day cutoff), the last five are
/// idle 3 days. Returns the old storage keys to hold a session lease under
/// and the old keys to hold an unresolved A2A context under.
async fn seed_bulk_fixture(
    js: &jetstream::Context,
    store: &SessionMetadataStore,
    sessions: usize,
    streamed: usize,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut leased = Vec::new();
    let mut context = Vec::new();
    for index in 0..sessions {
        let agent = if index % 2 == 0 { "alpha" } else { "beta" };
        let session_id = format!("s{index:03}");
        let storage_key = seed_session(js, store, agent, &session_id, index < streamed).await?;
        if index < 5 {
            backdate_activity(store, &storage_key, 40).await?;
            if index < 2 {
                leased.push(storage_key);
            } else if index == 2 {
                context.push(storage_key);
            }
        } else if index >= sessions - 5 {
            backdate_activity(store, &storage_key, 3).await?;
        }
    }
    Ok((leased, context))
}

/// The lease bucket, created the way the lease helpers expect it.
async fn create_lease_bucket(
    js: &jetstream::Context,
    lease_config: &NatsLeaseConfig,
) -> Result<kv::Store> {
    Ok(js
        .create_key_value(kv::Config {
            bucket: lease_config.bucket.clone(),
            history: 1,
            num_replicas: 1,
            ..Default::default()
        })
        .await?)
}

/// An unresolved A2A context: a hold the deletion fence would also see.
async fn put_unresolved_context(store: &SessionMetadataStore, storage_key: &str) -> Result<()> {
    store
        .kv_store()
        .put(
            format!("sessions/{storage_key}/a2a/context"),
            br#"{"state":{"active":{"kind":"busy"}}}"#.to_vec().into(),
        )
        .await?;
    Ok(())
}

/// Bare streams with no subjects, enough of them to push the stream listing
/// across a page boundary: the seed stream count alone stays well below any
/// plausible page size.
async fn create_filler_streams(js: &jetstream::Context, fillers: usize) -> Result<()> {
    for index in 0..fillers {
        js.create_stream(async_nats::jetstream::stream::Config {
            name: format!("ZZFILL_{index:03}"),
            ..Default::default()
        })
        .await?;
    }
    Ok(())
}

/// Subscribe to the JetStream API and direct-get request subjects. The pair
/// must exist before the collector runs; `collect_request_subjects` drains
/// them once it has finished.
async fn subscribe_requests(
    client: &async_nats::Client,
) -> Result<(async_nats::Subscriber, async_nats::Subscriber)> {
    let api = client.subscribe("$JS.API.>").await?;
    let direct = client.subscribe("$JS.DIRECT.>").await?;
    client.flush().await?;
    Ok((api, direct))
}

/// Drain both request subscriptions once the collector has finished.
async fn collect_request_subjects(
    mut api: async_nats::Subscriber,
    mut direct: async_nats::Subscriber,
) -> Result<Vec<String>> {
    let mut subjects = Vec::new();
    for subscription in [&mut api, &mut direct] {
        while let Ok(Some(message)) =
            tokio::time::timeout(StdDuration::from_millis(150), subscription.next()).await
        {
            subjects.push(message.subject.to_string());
        }
    }
    Ok(subjects)
}

/// Two live journal rows plus one deleted row whose marker must not count.
/// The deleted row's tombstone lives under `deleted/`, outside the session's
/// rows, and its old key's marker does not count either.
async fn seed_journal_rows(js: &jetstream::Context, storage_key: &str) -> Result<()> {
    let journal_bucket = js
        .create_key_value(kv::Config {
            bucket: harnx_toolset_server::invocation_journal::BUCKET.to_string(),
            history: 1,
            num_replicas: 1,
            ..Default::default()
        })
        .await?;
    for (round, call) in [(0, "a"), (1, "b")] {
        journal_bucket
            .put(
                format!("sessions.{storage_key}.{round}.{call}.{call}"),
                b"journal-row".to_vec().into(),
            )
            .await?;
    }
    let deleted_row = format!("sessions.{storage_key}.2.c.c");
    journal_bucket
        .put(&deleted_row, b"to-delete".to_vec().into())
        .await?;
    journal_bucket.delete(&deleted_row).await?;
    Ok(())
}

/// One live and one deleted checkpoint record under the session's hashed
/// prefix: the listing keeps delete markers, so only Put revisions count.
async fn seed_checkpoint_records(js: &jetstream::Context, storage_key: &str) -> Result<()> {
    let checkpoints_bucket = js
        .create_key_value(kv::Config {
            bucket: "harnx_interaction_checkpoints".to_string(),
            history: 1,
            num_replicas: 1,
            ..Default::default()
        })
        .await?;
    let hash = harnx_core::crypto::sha256(storage_key);
    checkpoints_bucket
        .put(
            format!("sessions.{hash}.record-1"),
            b"checkpoint-record".to_vec().into(),
        )
        .await?;
    let deleted = format!("sessions.{hash}.record-2");
    checkpoints_bucket
        .put(&deleted, b"to-delete".to_vec().into())
        .await?;
    checkpoints_bucket.delete(&deleted).await?;
    Ok(())
}

/// The bulk snapshot path must read everything with requests that scale with
/// listing pages, not with sessions: one metadata-snapshot consumer, one
/// lease-snapshot consumer, a paged stream listing, and no per-session KV
/// reads at all. Seeding creates far more broker state than the collector is
/// allowed to issue requests for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_cluster_stats_snapshot_requests_scale_with_pages_not_sessions() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        // nats-server binary unavailable: gated skip, same as every other
        // broker test in this suite.
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let sessions = 60;
    let streamed = 20;
    let fillers = 300;
    let (old_leased, old_context) = seed_bulk_fixture(&js, &store, sessions, streamed).await?;
    let lease_config = NatsLeaseConfig::default();
    let leases = create_lease_bucket(&js, &lease_config).await?;
    for storage_key in &old_leased {
        put_lease(&leases, &lease_config.key_for_session(storage_key)).await?;
    }
    for storage_key in &old_context {
        put_unresolved_context(&store, storage_key).await?;
    }
    create_filler_streams(&js, fillers).await?;

    let (api, direct) = subscribe_requests(&client).await?;
    let inputs = snapshot::read_cluster_inputs(&js).await?;
    let subjects = collect_request_subjects(api, direct).await?;

    assert_snapshot_scope(&inputs, sessions, streamed);
    let scope = aggregate::ReportScope::from_inputs(&inputs, Utc::now(), Some(30));
    let report = aggregate::build_cluster_report("test", &scope, inputs.shared_buckets.clone());
    assert_cluster_totals(&report, &inputs.shared_buckets, sessions, streamed);
    assert_request_shape(&subjects, sessions, streamed, fillers);
    drop(server);
    Ok(())
}

/// The bulk snapshot read every session and every session stream.
fn assert_snapshot_scope(inputs: &snapshot::ClusterInputs, sessions: usize, streamed: usize) {
    assert_eq!(
        inputs.sessions.len(),
        sessions,
        "every session snapshot-read"
    );
    assert_eq!(
        inputs.streams.len(),
        streamed,
        "every session stream listed"
    );
}

/// Cluster totals reconcile: per-agent split, age buckets, shared buckets and
/// snapshot-estimated retention with lease and A2A holds.
fn assert_cluster_totals(
    report: &ClusterStatsReport,
    shared_buckets: &[SharedBucketReport],
    sessions: usize,
    streamed: usize,
) {
    assert_eq!(report.session_count, sessions as u64);
    assert_eq!(report.agents.len(), 2);
    assert_eq!(report.streams.matched.count, streamed as u64);
    assert_eq!(report.streams.total.count, streamed as u64);
    assert_eq!(
        report.streams.metadata_only_sessions,
        (sessions - streamed) as u64
    );
    let age_counts = report.activity.age_counts;
    assert_eq!(age_counts.over_30_days, 5, "five sessions idle 40 days");
    assert_eq!(age_counts.one_to_7_days, 5, "five sessions idle 3 days");
    assert_eq!(age_counts.under_1_day, 50);
    assert!(report
        .activity
        .oldest_effective_activity
        .is_some_and(|oldest| (Utc::now() - oldest) > Duration::days(39)));
    let by_bucket = |bucket: &str| {
        shared_buckets
            .iter()
            .find(|report| report.bucket == bucket)
            .unwrap_or_else(|| panic!("missing shared bucket report for {bucket}"))
    };
    assert!(by_bucket("harnx_sessions").present);
    assert!(by_bucket("harnx_leases").present);
    assert!(!by_bucket("harnx_tool_invocations").present);
    assert!(!by_bucket("harnx_execution_control").present);
    // Snapshot-estimated retention: 5 age-expired, 2 lease-held, 1 held by an
    // unresolved A2A context, 2 eligible.
    assert_eq!(report.retention.age_expired, 5);
    assert_eq!(report.retention.held, 3);
    assert_eq!(report.retention.eligible, 2);
    assert_eq!(report.retention.held_session_lease, 2);
    assert_eq!(report.retention.held_unresolved_context, 1);
    assert_eq!(
        report.retention.age_expired,
        report.retention.held + report.retention.eligible
    );
}

/// Request-shape evidence: no per-session KV reads (direct get or
/// last-message get), at most the two snapshot consumers, and the paged
/// stream listing crossing a page boundary, all while the broker holds the
/// seeded session count. A per-session fan-out would need one request per
/// session.
fn assert_request_shape(subjects: &[String], sessions: usize, streamed: usize, fillers: usize) {
    let per_session_reads = subjects
        .iter()
        .filter(|subject| subject.contains("MSG.GET") || subject.starts_with("$JS.DIRECT.GET"))
        .count();
    assert_eq!(
        per_session_reads, 0,
        "the aggregate path must not read any single key: {subjects:#?}"
    );
    let consumer_creates = subjects
        .iter()
        .filter(|subject| subject.contains("CONSUMER.CREATE"))
        .count();
    assert!(
        consumer_creates <= 2,
        "one consumer per bulk snapshot, not per session: {subjects:#?}"
    );
    let stream_lists = subjects
        .iter()
        .filter(|subject| subject.contains("STREAM.LIST"))
        .count();
    assert!(
        stream_lists >= 2,
        "the stream listing crossed a page boundary: {subjects:#?}"
    );
    let total_streams = (fillers + streamed + 2) as u64;
    assert!(
        subjects.len() as u64 <= total_streams / 10,
        "requests must stay bounded far below the session/stream count: {} requests for {sessions} sessions",
        subjects.len()
    );
}

/// An empty cluster is a valid, read-only state: every shared bucket reports
/// `present=false`, nothing is created, and an unknown session is a clear
/// error, not a fabricated zero report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_stats_absent_buckets_report_missing_without_creating() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    let inputs = snapshot::read_cluster_inputs(&js).await?;
    assert!(inputs.sessions.is_empty());
    assert!(inputs.streams.is_empty());
    assert!(inputs.live_lease_keys.is_empty());
    assert_eq!(inputs.shared_buckets.len(), 7);
    assert!(inputs.shared_buckets.iter().all(|bucket| !bucket.present));
    let scope = aggregate::ReportScope::from_inputs(&inputs, Utc::now(), Some(30));
    let report = aggregate::build_cluster_report("test", &scope, inputs.shared_buckets.clone());
    assert_eq!(report.session_count, 0);
    assert!(report.activity.oldest_effective_activity.is_none());
    assert_eq!(report.retention.age_expired, 0);
    // The collector created nothing: the broker still has no streams.
    assert_eq!(
        count_streams(&js).await?,
        0,
        "read-only collection must not create streams"
    );
    let detail = super::detail::collect_session_detail("test", &js, "alpha", "unknown")
        .await
        .expect_err("unknown session must fail");
    assert!(
        detail.to_string().contains("not found"),
        "clear not-found error, got: {detail:#}"
    );
    assert_eq!(
        count_streams(&js).await?,
        0,
        "a failed lookup must not create streams either"
    );
    Ok(())
}

/// How many streams the broker holds, to prove read-only collection creates
/// nothing.
async fn count_streams(js: &jetstream::Context) -> Result<usize> {
    let mut names = js.stream_names();
    let mut listed = 0;
    while let Some(name) = names.next().await {
        name.context("stream name")?;
        listed += 1;
    }
    Ok(listed)
}

/// The single-session view spends targeted requests by design: it reads the
/// named session's canonical metadata, activity, stream, leases, A2A facts
/// and owned-resource counts, without writing or creating anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_stats_detail_reports_targeted_facts() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let storage_key = seed_session(&js, &store, "alpha", "detail-1", false).await?;
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key.clone(), 1);
    log.append_event_async(&user_event("first")).await?;
    log.append_event_async(&user_event("second")).await?;

    // A live session lease and a live scoped A2A lease.
    let lease_config = NatsLeaseConfig::default();
    let leases = create_lease_bucket(&js, &lease_config).await?;
    put_lease(&leases, &lease_config.key_for_session(&storage_key)).await?;
    put_lease(&leases, &format!("sessions/{storage_key}/a2a/lock")).await?;
    put_unresolved_context(&store, &storage_key).await?;
    seed_journal_rows(&js, &storage_key).await?;
    seed_checkpoint_records(&js, &storage_key).await?;

    let activity_before = store.get_activity(&storage_key).await?;
    let detail = super::detail::collect_session_detail("test", &js, "alpha", "detail-1").await?;
    assert_detail_facts(&detail, &storage_key);
    let activity_after = store.get_activity(&storage_key).await?;
    assert_eq!(
        activity_before, activity_after,
        "read-only collection must not touch the activity record"
    );
    Ok(())
}

/// The targeted facts: stream shape, holds split by reason, owned live
/// counts, and absent resource buckets reported as absent, never fabricated
/// as zero-sized.
fn assert_detail_facts(detail: &SessionStatsDetail, storage_key: &str) {
    assert_eq!(detail.storage_key, storage_key);
    assert!(detail.metadata_revision >= 1);
    assert!(detail.activity.is_some());
    assert!(detail.stream_present);
    let stream = detail.stream.as_ref().expect("session stream");
    assert_eq!(stream.messages, 2);
    assert!(stream.bytes > 0);
    assert!(stream.last_message_time.is_some());
    assert!(stream.first_sequence.is_some());
    assert!(
        detail.holds.session_lease,
        "live session lease is reported: {}",
        serde_json::to_string(&detail.holds).unwrap_or_default()
    );
    assert!(detail.holds.a2a_lease);
    assert!(detail.holds.a2a_unresolved_context);
    assert!(!detail.holds.a2a_unsettled_first_reservation);
    assert_eq!(
        (detail.journal_rows.present, detail.journal_rows.count),
        (true, 2),
        "two live journal rows"
    );
    assert_eq!(
        (detail.checkpoints.present, detail.checkpoints.count),
        (true, 1),
        "one live checkpoint record; the deleted one's marker does not count"
    );
    // Plans and attachments buckets were never created: absent, not zero-
    // sized, and the collector must not create them to find that out.
    assert_eq!(
        (
            detail.plans.present,
            detail.plans.plans,
            detail.plans.documents
        ),
        (false, 0, 0)
    );
    assert_eq!(
        (detail.attachments.present, detail.attachments.count),
        (false, 0)
    );
}

/// A released lease leaves a Delete marker behind: the last record the bulk
/// lease snapshot delivers can be a marker, so the read must terminate on a
/// trailing marker instead of waiting for a further publish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_cluster_stats_terminates_on_released_lease_marker() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    let lease_config = NatsLeaseConfig::default();
    let leases = create_lease_bucket(&js, &lease_config).await?;
    // Put then delete the only lease: its marker is the snapshot's sole
    // delivered record. Before the pending==0 check covered markers too,
    // this call never returned and bulk stats hung on a quiet bucket.
    let released = lease_config.key_for_session("alpha/released");
    put_lease(&leases, &released).await?;
    leases.delete(&released).await?;
    let keys = tokio::time::timeout(
        StdDuration::from_secs(60),
        crate::nats_lease::snapshot_live_lease_keys(&js),
    )
    .await
    .context("the lease snapshot must terminate on a trailing Delete marker")??;
    assert!(
        keys.map(|keys| keys.is_empty()).unwrap_or(true),
        "a released lease is not live"
    );

    // A live lease still shows up after the marker was the last record.
    let live = lease_config.key_for_session("alpha/live");
    put_lease(&leases, &live).await?;
    let keys = tokio::time::timeout(
        StdDuration::from_secs(60),
        crate::nats_lease::snapshot_live_lease_keys(&js),
    )
    .await
    .context("the lease snapshot must terminate with live leases present")??;
    let keys = keys.unwrap_or_default();
    assert!(keys.contains(&live), "the live lease is live: {keys:?}");
    assert!(
        !keys.contains(&released),
        "the released lease is gone: {keys:?}"
    );
    Ok(())
}

/// A Config aimed at the spawned broker, with retention configured so the
/// report's retention estimate is enabled.
fn stats_config(url: &str) -> crate::config::Config {
    let mut config = crate::config::Config {
        nats_servers: vec![crate::config::NatsServerConfig {
            user_id: None,
            name: "stats".to_string(),
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
    };
    config.data.cleanup_remote_sessions_days = Some(30);
    config
}

/// The public collector entry points return the same views the pure builders
/// produce: the cluster view joins inline sessions and classifies orphan
/// streams, the agent view counts only streams the agent's sessions own, and
/// the session view resolves one named session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_public_collectors_report_cluster_agent_and_session() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = stats_config(server.url());
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let streamed = seed_session(&js, &store, "alpha", "s1", true).await?;
    seed_session(&js, &store, "alpha", "s2", false).await?;
    // An inline session: metadata-only, and never part of a named agent's
    // scope.
    let inline = SessionMetadata::new(
        "inline-1",
        SessionInitializer::inline(
            "instructions",
            AgentVariables::default(),
            crate::nats_session_metadata::SessionOverrides::default(),
        ),
    );
    store.create(&inline).await?;
    store.touch_activity(&inline.storage_key()).await?;
    // A real session stream whose session has no metadata: orphan-mapped.
    let orphan = NatsSessionLog::new_with_replicas(js.clone(), "orphan-key".to_string(), 1);
    orphan
        .append_event_async(&user_event("orphan event"))
        .await?;

    let cluster = super::collect_cluster_stats(&config, "stats").await?;
    assert_eq!(
        cluster.session_count, 3,
        "inline sessions join the unfiltered cluster view"
    );
    assert_eq!(cluster.agents.len(), 2, "named agent plus the inline group");
    assert!(cluster
        .agents
        .iter()
        .any(|agent| matches!(agent.agent, StatsAgentSource::Inline)));
    assert_eq!(cluster.streams.total.count, 2, "seeded stream plus orphan");
    assert_eq!(cluster.streams.matched.count, 1);
    assert_eq!(cluster.streams.orphan_mapped.count, 1);
    assert_eq!(cluster.streams.metadata_only_sessions, 2);

    let agent = super::collect_agent_stats(&config, "stats", "alpha").await?;
    assert_eq!(
        agent.session_count, 2,
        "inline sessions never join an agent view"
    );
    assert_eq!(agent.metadata_only_sessions, 1);
    assert_eq!(
        agent.streams.count, 1,
        "a metadata-only session owns no stream and must not count as one"
    );
    assert_eq!(agent.streams.messages, 1);

    let detail = super::collect_session_stats(&config, "stats", "alpha", "s1").await?;
    assert_eq!(detail.storage_key, streamed);
    assert!(detail.stream_present);
    assert!(detail.stream.as_ref().expect("session stream").messages >= 1);

    let missing = super::collect_session_stats(&config, "stats", "alpha", "unknown")
        .await
        .expect_err("unknown session must fail through the public path");
    assert!(
        missing.to_string().contains("not found"),
        "clear not-found error, got: {missing:#}"
    );
    Ok(())
}

/// Malformed activity JSON must not fail the targeted detail view: the
/// aggregate path tolerates it, so the detail view reports the record as
/// absent instead of erroring while cleanup still works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_stats_detail_tolerates_malformed_activity() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let storage_key = seed_session(&js, &store, "alpha", "bad-activity", false).await?;
    store
        .kv_store()
        .put(activity_key(&storage_key), b"not json".to_vec().into())
        .await?;

    let detail =
        super::detail::collect_session_detail("test", &js, "alpha", "bad-activity").await?;
    assert_eq!(detail.storage_key, storage_key);
    assert!(
        detail.activity.is_none(),
        "malformed activity is reported as absent, not an error"
    );
    // The aggregate path agrees: the same malformed record falls back to
    // metadata created_at instead of failing the bulk join.
    let inputs = snapshot::read_cluster_inputs(&js).await?;
    assert_eq!(inputs.sessions.len(), 1);
    assert_eq!(
        inputs.sessions[0].effective_activity, detail.metadata_created_at,
        "aggregate and cleanup share the created_at fallback"
    );
    Ok(())
}

/// A metadata-only session with populated owned resources: plans, attachments,
/// journal and checkpoint counts populate from their owning buckets while the
/// stream stays absent, and no zero-sized stream is fabricated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_stats_detail_reports_resources_without_stream() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let storage_key = seed_session(&js, &store, "alpha", "detail-res", false).await?;
    seed_journal_rows(&js, &storage_key).await?;
    seed_checkpoint_records(&js, &storage_key).await?;

    // One plan holding an index document and one task document.
    let plans = harnx_blob_store::ensure_plans_bucket(&js, 1).await?;
    let session_ref =
        harnx_core::cid_url::SessionRef::new(Some("alpha".to_string()), "detail-res".to_string())?;
    for (item, body) in [
        (
            harnx_core::cid_url::PlanItem::Index,
            r#"{"title":"my plan"}"#,
        ),
        (
            harnx_core::cid_url::PlanItem::Task("t01".to_string()),
            r#"{"status":"open"}"#,
        ),
    ] {
        let url = harnx_core::cid_url::CidUrl::Plan {
            session: session_ref.clone(),
            slug: "my-plan".to_string(),
            item,
        };
        harnx_blob_store::plans::create_document(&plans, &url, body).await?;
    }

    // Two media objects of 12 and 1 bytes under the same owner.
    let attachments = harnx_blob_store::create_or_open_attachments_bucket(&js, 1).await?;
    for media in [b"twelve bytes".as_slice(), b"x".as_slice()] {
        let hash = harnx_core::crypto::sha256(std::str::from_utf8(media).unwrap());
        let url = harnx_blob_store::media_cid_url(&session_ref, &hash);
        harnx_blob_store::put_media(&attachments, &url, media, "text/plain").await?;
    }

    let detail = super::detail::collect_session_detail("test", &js, "alpha", "detail-res").await?;
    assert_eq!(detail.storage_key, storage_key);
    assert!(!detail.stream_present, "no session stream was created");
    assert!(detail.stream.is_none(), "no fabricated zero-sized stream");
    assert_eq!(
        (
            detail.plans.present,
            detail.plans.plans,
            detail.plans.documents
        ),
        (true, 1, 2),
        "one plan with an index and a task document"
    );
    assert_eq!(
        (
            detail.attachments.present,
            detail.attachments.count,
            detail.attachments.logical_bytes
        ),
        (true, 2, 13),
        "two canonical objects, logical bytes from object metadata"
    );
    assert_eq!(
        (detail.journal_rows.present, detail.journal_rows.count),
        (true, 2),
        "two live journal rows"
    );
    assert_eq!(
        (detail.checkpoints.present, detail.checkpoints.count),
        (true, 1),
        "one live checkpoint record"
    );
    Ok(())
}

/// Publish a store-style marker for `key` whose own TTL is short, then wait
/// for nats-server's expiry sweep to replace it with the artifact a harnx
/// tombstone leaves behind once its own TTL runs out: the latest revision
/// carries `Nats-Marker-Reason` and no `KV-Operation` header. Returns the
/// marker's reason so the test can pin what the server actually wrote.
async fn publish_marker_await_server_expiry(
    js: &jetstream::Context,
    bucket: &str,
    key: &str,
    operation: &str,
) -> Result<String> {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("KV-Operation", operation);
    headers.insert(async_nats::header::NATS_MESSAGE_TTL, "2s");
    let subject = format!("$KV.{bucket}.{key}");
    js.publish_with_headers(subject.clone(), headers, Bytes::new())
        .await?
        .await?;
    let stream_handle = js.get_stream(format!("KV_{bucket}")).await?;
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(30);
    loop {
        let message = stream_handle
            .get_last_raw_message_by_subject(&subject)
            .await
            .context("read the latest revision while waiting for expiry")?;
        if let Some(reason) = message
            .headers
            .get(async_nats::header::NATS_MARKER_REASON)
            .map(|reason| reason.as_str().to_owned())
        {
            ensure!(
                message.headers.get("KV-Operation").is_none(),
                "the server's expiry marker still carries a KV-Operation header"
            );
            return Ok(reason);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "nats-server never replaced the TTL'd {operation} marker for {key}"
        );
        tokio::time::sleep(StdDuration::from_millis(250)).await;
    }
}

/// A deleted session's tombstone expires after `DELETION_MARKER_TTL`; the
/// revision nats-server leaves in its place is a marker with a reason header
/// only. The bulk metadata snapshot must decode it as not-a-Put — the
/// leader-read decoder — instead of treating the session as live or failing
/// the whole snapshot on its empty payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_stats_snapshot_decodes_server_expiry_marker_metadata() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let live = seed_session(&js, &store, "alpha", "live", false).await?;
    let expired = seed_session(&js, &store, "alpha", "expired", false).await?;

    let reason =
        publish_marker_await_server_expiry(&js, "harnx_sessions", &metadata_key(&expired), "PURGE")
            .await?;
    // The server's own KV view agrees the key is gone; the shared decoder
    // must reach the same conclusion from the same headers.
    assert_eq!(reason, "MaxAge", "the per-message TTL expiry reason");
    assert!(store
        .kv_store()
        .get(metadata_key(&expired))
        .await?
        .is_none());

    let snapshot = store.snapshot().await?;
    let entry = snapshot
        .get(&metadata_key(&expired))
        .expect("the expiry marker is a fact, not an absent key");
    assert_ne!(
        entry.operation,
        kv::Operation::Put,
        "a server expiry marker must not decode as a live revision"
    );
    assert_eq!(
        snapshot
            .get(&metadata_key(&live))
            .expect("live session")
            .operation,
        kv::Operation::Put
    );

    let inputs = snapshot::read_cluster_inputs(&js).await?;
    let storage_keys: Vec<&str> = inputs
        .sessions
        .iter()
        .map(|session| session.storage_key.as_str())
        .collect();
    assert_eq!(
        storage_keys,
        vec![live.as_str()],
        "only the live session counts; the tombstone-expired session is gone"
    );
    drop(server);
    Ok(())
}

/// A released lease's Delete marker expires after the bucket's tombstone TTL;
/// the revision the server leaves in its place must not count as a live
/// lease in the bulk lease snapshot, while live leases still do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_lease_stats_snapshot_decodes_server_expiry_marker_leases() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = jetstream::new(client.clone());
    // The production bucket creator: `limit_markers` is what lets the server
    // expire markers and leave `Nats-Marker-Reason` revisions behind.
    let lease_config = NatsLeaseConfig::default();
    let leases = ensure_lease_bucket(&js, &lease_config).await?;
    let live = lease_config.key_for_session("alpha/live");
    let released = lease_config.key_for_session("alpha/released");
    put_lease(&leases, &live).await?;
    put_lease(&leases, &released).await?;

    let reason = publish_marker_await_server_expiry(&js, "harnx_leases", &released, "DEL").await?;
    assert_eq!(reason, "MaxAge", "the per-message TTL expiry reason");

    let keys = tokio::time::timeout(
        StdDuration::from_secs(60),
        crate::nats_lease::snapshot_live_lease_keys(&js),
    )
    .await
    .context("the lease snapshot must terminate")??
    .unwrap_or_default();
    assert!(keys.contains(&live), "the live lease stays live: {keys:?}");
    assert!(
        !keys.contains(&released),
        "a lease whose marker the server expired is not live: {keys:?}"
    );
    drop(server);
    Ok(())
}
