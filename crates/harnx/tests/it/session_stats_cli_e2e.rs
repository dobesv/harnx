//! Broker-spawned end-to-end tests for the #2923 CLI surfaces: `harnx info
//! sessions` (cluster/agent/session views, text and JSON) and `harnx delete
//! expired-sessions` (preview, deletion, validation, contention). Each test
//! runs the real `harnx` binary against an isolated NATS broker through an
//! explicitly named cluster (`nats_servers/stats.yaml`), never the built-in
//! local key, and seeds state in-process with the runtime's own stores.

use anyhow::{Context as _, Result};
use chrono::{TimeZone, Utc};
use harnx_core::require_nextest;
use harnx_runtime::config::{Config, NatsServerConfig};
use harnx_runtime::manual_session_cleanup::DEFAULT_MANUAL_CLEANUP_MAX;
use harnx_runtime::nats_session_log::stream_name_for_session;
use harnx_runtime::nats_session_metadata::{
    activity_key, SessionActivity, SessionAgentSource, SessionInitializer, SessionMetadata,
    SessionMetadataStore,
};
use harnx_runtime::remote_session_cleanup::{acquire_session_gc_lease, GC_LAST_RUN_EPOCH_HOUR_KEY};
use std::process::Output;

use crate::cli_runner::run_harnx;

/// The named cluster key every fixture writes and every command targets.
const CLUSTER: &str = "stats";

/// One isolated broker plus a config directory that routes the `stats`
/// cluster key to it. `config/config.yaml` is written on demand: without it
/// retention is unset, which the validation tests rely on.
struct CliFixture {
    root: tempfile::TempDir,
    /// Keeps the spawned broker alive for the whole test: dropping the handle
    /// kills the nats-server process.
    _server: Option<harnx_test_bins::NatsServerHandle>,
    /// `None` for offline fixtures; only the CLI runs there.
    jetstream: Option<async_nats::jetstream::Context>,
    config: Config,
}

impl CliFixture {
    async fn new(server: harnx_test_bins::NatsServerHandle) -> Result<Self> {
        let url = server.url().to_string();
        let root = Self::write_cluster_config(&url)?;
        let config = fixture_config(&url);
        let jetstream = config.nats_jetstream(CLUSTER).await?;
        Ok(Self {
            root,
            _server: Some(server),
            jetstream: Some(jetstream),
            config,
        })
    }

    // qual:test_helper
    /// A fixture against a dead endpoint: validation failures must occur
    /// without the CLI ever connecting, so the fixture never connects either.
    fn offline(url: &str) -> Result<Self> {
        let root = Self::write_cluster_config(url)?;
        Ok(Self {
            root,
            _server: None,
            jetstream: None,
            config: fixture_config(url),
        })
    }

    /// The online fixture's broker handle; offline fixtures have none.
    fn js(&self) -> &async_nats::jetstream::Context {
        self.jetstream
            .as_ref()
            .expect("offline fixture has no broker handle")
    }

    fn write_cluster_config(url: &str) -> Result<tempfile::TempDir> {
        let root = tempfile::tempdir()?;
        let servers = root.path().join("config/nats_servers");
        std::fs::create_dir_all(&servers)?;
        std::fs::write(servers.join("stats.yaml"), format!("url: {url}\n"))?;
        Ok(root)
    }

    // qual:test_helper
    /// Write the fixture's main config, e.g. `cleanup_remote_sessions_days: 0`.
    fn set_main_config(&self, body: &str) {
        std::fs::write(self.root.path().join("config/config.yaml"), body).unwrap();
    }

    // qual:test_helper
    fn run(&self, args: &[&str]) -> Result<Output> {
        run_harnx(self.root.path(), args, &[]).with_context(|| format!("run {args:?}"))
    }

    /// `harnx info sessions --cluster stats` plus extra args.
    fn info_sessions(&self, extra: &[&str]) -> Result<Output> {
        let mut args = vec!["info", "sessions", "--cluster", CLUSTER];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    /// `harnx delete expired-sessions --cluster stats` plus extra args.
    fn delete_expired(&self, extra: &[&str]) -> Result<Output> {
        let mut args = vec!["delete", "expired-sessions", "--cluster", CLUSTER];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    // qual:test_helper
    async fn metadata_count(&self) -> Result<usize> {
        Ok(SessionMetadataStore::ensure(self.js(), 1)
            .await?
            .list()
            .await?
            .len())
    }

    // qual:test_helper
    async fn session_exists(&self, storage_key: &str) -> Result<bool> {
        let store = SessionMetadataStore::ensure(self.js(), 1).await?;
        Ok(store
            .list()
            .await?
            .iter()
            .any(|listed| listed.metadata.storage_key() == storage_key))
    }

    // qual:test_helper
    async fn stream_exists(&self, storage_key: &str) -> Result<bool> {
        Ok(self
            .js()
            .get_stream(stream_name_for_session(storage_key))
            .await
            .is_ok())
    }

    /// The worker GC's epoch-hour marker must never be written by the manual
    /// pass; it lives in the `harnx_leases` KV bucket the lease helper uses.
    // qual:test_helper
    async fn epoch_hour_marker_absent(&self) -> Result<bool> {
        match self.config.nats_kv_bucket(CLUSTER, "harnx_leases").await {
            Ok(store) => Ok(store.get(GC_LAST_RUN_EPOCH_HOUR_KEY).await?.is_none()),
            Err(_) => Ok(true),
        }
    }
}

/// Seed ages: comfortably past the 30-day retention used throughout, just
/// inside it, and active now.
const AGE_STALE_SECS: i64 = 40 * 24 * 3600;
const AGE_FRESH_SECS: i64 = 3600;
const AGE_ACTIVE_SECS: i64 = 60;

/// The standard broker test preamble: require nextest, spawn an isolated
/// NATS server, and wire the `stats` cluster fixture to it. `None` means
/// no nats-server binary; the caller skips like it always did.
// qual:test_helper
async fn broker_fixture() -> Result<Option<CliFixture>> {
    require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(None);
    };
    Ok(Some(CliFixture::new(server).await?))
}

// qual:test_helper
fn fixture_config(url: &str) -> Config {
    Config {
        nats_servers: vec![NatsServerConfig {
            user_id: None,
            name: CLUSTER.to_string(),
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

struct Seeded {
    storage_key: String,
}

/// Seed several sessions for one agent at the same activity time; the
/// result order matches the id order so tests can index deterministically.
// qual:test_helper
async fn seed_sessions(
    fixture: &CliFixture,
    agent: &str,
    ids: &[&str],
    activity_secs: i64,
    with_stream: bool,
) -> Result<Vec<Seeded>> {
    let mut seeded = Vec::new();
    for id in ids {
        seeded.push(seed_session(fixture.js(), agent, id, activity_secs, with_stream).await?);
    }
    Ok(seeded)
}

/// Seed one session record: canonical metadata, an activity record at
/// `activity_secs` (the cleanup/age semantics source) and, optionally, a
/// session stream with one message.
// qual:test_helper
async fn seed_session(
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
    seed_record(jetstream, metadata, activity_secs, with_stream).await
}

/// Seed one inline-agent session (metadata + activity, no stream).
// qual:test_helper
async fn seed_inline_session(
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
    seed_record(jetstream, metadata, activity_secs, false).await
}

async fn seed_record(
    jetstream: &async_nats::jetstream::Context,
    metadata: SessionMetadata,
    activity_secs: i64,
    with_stream: bool,
) -> Result<Seeded> {
    let store = SessionMetadataStore::ensure(jetstream, 1).await?;
    let storage_key = metadata.storage_key();
    store
        .create(&metadata)
        .await?
        .with_context(|| format!("seed metadata for '{storage_key}'"))?;
    store
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
    if with_stream {
        use async_nats::jetstream::stream;
        jetstream
            .create_stream(stream::Config {
                name: stream_name_for_session(&storage_key),
                subjects: vec![format!("sessions.{storage_key}.>")],
                storage: stream::StorageType::File,
                ..Default::default()
            })
            .await?;
        jetstream
            .publish(format!("sessions.{storage_key}.event"), "x".into())
            .await?
            .await?;
    }
    Ok(Seeded { storage_key })
}

// qual:test_helper
fn secs_ago(secs: i64) -> i64 {
    Utc::now().timestamp() - secs
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

// qual:test_helper
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

// qual:test_helper
fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected nonzero exit\nstdout: {}",
        stdout(output)
    );
}

// qual:test_helper
fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).expect("stdout must be pure JSON")
}

// ---------------------------------------------------------------------------
// `harnx info sessions`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_info_sessions_cluster_report_text_and_json() -> Result<()> {
    let Some(fixture) = broker_fixture().await? else {
        return Ok(());
    };
    fixture.set_main_config("cleanup_remote_sessions_days: 30\n");
    let stale = secs_ago(AGE_STALE_SECS);
    let fresh = secs_ago(AGE_FRESH_SECS);
    seed_session(fixture.js(), "alpha", "old-1", stale, true).await?;
    seed_session(fixture.js(), "alpha", "new-1", fresh, true).await?;
    seed_session(fixture.js(), "beta", "beta-1", fresh, true).await?;
    seed_inline_session(fixture.js(), "inline-1", stale).await?;

    let text = fixture.info_sessions(&[])?;
    assert_success(&text);
    let rendered = stdout(&text);
    assert!(
        rendered.contains("Session storage on cluster 'stats'"),
        "{rendered}"
    );
    assert!(
        rendered.contains("sessions: 4 metadata-backed"),
        "{rendered}"
    );
    assert!(
        rendered.contains("retention: 30 days (enabled)"),
        "{rendered}"
    );
    assert!(rendered.contains("alpha: 2 sessions"), "{rendered}");
    assert!(rendered.contains("beta: 1 sessions"), "{rendered}");

    let output = fixture.info_sessions(&["--json"])?;
    assert_success(&output);
    let report = json_of(&output);
    assert_eq!(report["view"], "cluster");
    assert_eq!(report["cluster"], "stats");
    assert_eq!(report["session_count"], 4);
    let streams = &report["streams"];
    // The three stream breakdowns must reconcile with the whole.
    for field in ["count", "bytes", "messages"] {
        assert_eq!(
            streams["total"][field].as_u64().unwrap(),
            streams["matched"][field].as_u64().unwrap()
                + streams["orphan_mapped"][field].as_u64().unwrap()
                + streams["orphan_unmapped"][field].as_u64().unwrap(),
            "breakdown {field} must reconcile with total"
        );
    }
    assert_eq!(streams["metadata_only_sessions"], 1);
    assert_eq!(report["activity"]["age_counts"]["under_1_day"], 2);
    assert_eq!(report["activity"]["age_counts"]["over_30_days"], 2);
    assert!(report["activity"]["oldest_effective_activity"].is_string());
    assert!(report["collected_at"].is_string());
    // All seven shared buckets, presence explicit, not fabricated sizes.
    let buckets = report["shared_buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 7);
    assert!(
        buckets
            .iter()
            .any(|bucket| bucket["stream"] == "KV_harnx_sessions" && bucket["present"] == true),
        "{buckets:?}"
    );
    // Agents sorted by descending stream bytes; identity is structured.
    let agents = report["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 3);
    assert!(
        agents[0]["streams"]["bytes"].as_u64().unwrap()
            >= agents[1]["streams"]["bytes"].as_u64().unwrap()
    );
    assert!(
        agents
            .iter()
            .any(|agent| agent["agent"] == serde_json::json!({"kind": "inline"})),
        "{agents:?}"
    );
    let retention = &report["retention"];
    assert_eq!(retention["configured_days"], 30);
    assert_eq!(retention["enabled"], true);
    assert_eq!(retention["age_expired"], 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_info_sessions_agent_and_session_scopes() -> Result<()> {
    let Some(fixture) = broker_fixture().await? else {
        return Ok(());
    };
    let stale = secs_ago(AGE_STALE_SECS);
    let fresh = secs_ago(AGE_ACTIVE_SECS);
    seed_session(fixture.js(), "alpha", "old-1", stale, true).await?;
    seed_session(fixture.js(), "alpha", "new-1", fresh, true).await?;
    seed_session(fixture.js(), "beta", "beta-1", stale, true).await?;
    seed_inline_session(fixture.js(), "inline-1", stale).await?;

    // Agent view: exact named-agent scope; beta and inline never join.
    let text = fixture.info_sessions(&["--agent", "alpha"])?;
    assert_success(&text);
    let rendered = stdout(&text);
    assert!(
        rendered.contains("Session storage for agent 'alpha'"),
        "{rendered}"
    );
    assert!(
        rendered.contains("sessions: 2 metadata-backed"),
        "{rendered}"
    );

    let output = fixture.info_sessions(&["--agent", "alpha", "--json"])?;
    assert_success(&output);
    let report = json_of(&output);
    assert_eq!(report["view"], "agent");
    assert_eq!(report["session_count"], 2);
    assert_eq!(report["metadata_only_sessions"], 0);
    assert!(report["largest_sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["session_id"].as_str().unwrap() != "beta-1"
            && entry["session_id"].as_str().unwrap() != "inline-1"));
    assert!(!report["largest_sessions"].as_array().unwrap().is_empty());

    // Single-session view: targeted detail for one alpha session.
    let output = fixture.info_sessions(&["--agent", "alpha", "old-1", "--json"])?;
    assert_success(&output);
    let detail = json_of(&output);
    assert_eq!(detail["view"], "session");
    assert_eq!(detail["session_id"], "old-1");
    assert_eq!(detail["metadata_revision"], 1);
    assert!(detail["metadata_created_at"].is_string());
    assert_eq!(detail["stream_present"], true);
    assert_eq!(detail["stream"]["messages"], 1);
    assert_eq!(detail["stream"]["first_sequence"], 1);
    assert_eq!(detail["journal_rows"]["present"], false);
    assert_eq!(detail["journal_rows"]["count"], 0);
    assert_eq!(detail["holds"]["session_lease"], false);

    // Text form of the same view stays compact and labeled.
    let text = fixture.info_sessions(&["--agent", "alpha", "old-1"])?;
    assert_success(&text);
    let rendered = stdout(&text);
    assert!(
        rendered.contains("Session storage for 'old-1' (agent 'alpha')"),
        "{rendered}"
    );
    assert!(rendered.contains("holds: session lease no"), "{rendered}");

    // A session id without --agent is rejected by the parser itself.
    let output = fixture.info_sessions(&["old-1"])?;
    assert_failure(&output);
    assert!(
        stderr(&output).contains("--agent"),
        "stderr: {}",
        stderr(&output)
    );

    // Unknown session: clear not-found error, no stdout.
    let output = fixture.info_sessions(&["--agent", "alpha", "no-such-session"])?;
    assert_failure(&output);
    assert!(stdout(&output).is_empty());
    assert!(
        stderr(&output).to_lowercase().contains("not found"),
        "stderr: {}",
        stderr(&output)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `harnx delete expired-sessions`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_delete_expired_sessions_dry_run_then_real_run() -> Result<()> {
    let Some(fixture) = broker_fixture().await? else {
        return Ok(());
    };
    let stale = secs_ago(AGE_STALE_SECS);
    let fresh = secs_ago(AGE_FRESH_SECS);
    let alpha_old = seed_session(fixture.js(), "alpha", "old-1", stale, true).await?;
    let beta_old = seed_session(fixture.js(), "beta", "old-2", stale, false).await?;
    seed_session(fixture.js(), "alpha", "new-1", fresh, true).await?;

    // Dry run: guards run, nothing is deleted, the report says what would go.
    let output = fixture.delete_expired(&["--days", "30", "--dry-run"])?;
    assert_success(&output);
    let rendered = stdout(&output);
    assert!(
        rendered.contains("dry run, nothing is deleted"),
        "{rendered}"
    );
    assert!(
        rendered.contains("scanned: 2 age-expired candidates"),
        "{rendered}"
    );
    assert!(rendered.contains("would delete: 2"), "{rendered}");
    assert!(rendered.contains(&alpha_old.storage_key), "{rendered}");
    assert_eq!(fixture.metadata_count().await?, 3);
    assert!(fixture.session_exists(&alpha_old.storage_key).await?);
    assert!(fixture.stream_exists(&alpha_old.storage_key).await?);
    assert!(fixture.epoch_hour_marker_absent().await?);

    // Real run with the same bound: both stale sessions go, the fresh one
    // stays, and no epoch-hour marker appears.
    let output = fixture.delete_expired(&["--days", "30"])?;
    assert_success(&output);
    let rendered = stdout(&output);
    assert!(rendered.contains("attempted: 2, deleted: 2"), "{rendered}");
    assert_eq!(fixture.metadata_count().await?, 1);
    assert!(!fixture.session_exists(&alpha_old.storage_key).await?);
    assert!(!fixture.stream_exists(&alpha_old.storage_key).await?);
    assert!(!fixture.session_exists(&beta_old.storage_key).await?);
    assert!(fixture.epoch_hour_marker_absent().await?);
    Ok(())
}

/// One plan document of invalid bytes under the session's owner prefix: the
/// deletion cascade reaches `delete_owner`, fails there, and records the
/// candidate as `FAILED` with possible partial effects. This is real broker
/// state a run can hit — a plan written by an older or corrupt writer — so
/// no test hook is needed to make the pass fail mid-cascade.
// qual:test_helper
async fn seed_poisoned_plan(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<()> {
    let plans = harnx_blob_store::ensure_plans_bucket(jetstream, 1).await?;
    plans
        .put(
            &format!("plan/{storage_key}/test/index.md"),
            vec![0xff].into(),
        )
        .await?;
    Ok(())
}

/// A pass that ran and completed, but had a per-session failure, is the
/// completed-but-failed outcome: the report still prints in full — summary,
/// both dispositions, the error hint — and the process exits 1. This is the
/// only path where a valid candidate's work is preserved alongside another
/// candidate's failure; pre-pass `Err` failures print no report at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_delete_expired_sessions_completed_failed_pass_prints_summary_and_exits_one(
) -> Result<()> {
    let Some(fixture) = broker_fixture().await? else {
        return Ok(());
    };
    fixture.set_main_config("cleanup_remote_sessions_days: 30\n");
    let stale = secs_ago(AGE_STALE_SECS);
    let clean = seed_session(fixture.js(), "alpha", "fail-clean", stale, true).await?;
    let poisoned = seed_session(fixture.js(), "beta", "fail-poisoned", stale, true).await?;
    seed_poisoned_plan(fixture.js(), &poisoned.storage_key).await?;

    let output = fixture.delete_expired(&[])?;
    // Not just nonzero: the command maps outcome.failed to exactly 1, and a
    // constant-zero or swallowed status would fail this.
    assert_eq!(
        output.status.code(),
        Some(1),
        "a completed pass with failures exits 1\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let rendered = stdout(&output);
    assert!(
        rendered.contains("attempted: 2, deleted: 1"),
        "summary keeps both candidates' work: {rendered}"
    );
    assert!(rendered.contains("errors: 1"), "{rendered}");
    assert!(
        rendered.contains(&format!("  {}: deleted", clean.storage_key)),
        "the completed deletion's disposition is preserved: {rendered}"
    );
    assert!(
        rendered.contains(&format!("  {}: FAILED", poisoned.storage_key)),
        "the failed candidate's disposition is preserved: {rendered}"
    );
    assert!(
        rendered.contains("[partial effects possible]"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Errors or possible partial effects occurred"),
        "the operator hint follows the dispositions: {rendered}"
    );

    // The report told the truth: the clean candidate is fully gone, the
    // poisoned candidate's stream was deleted but its metadata survives
    // for a retry — the same partial effects the runtime reports.
    assert!(!fixture.stream_exists(&clean.storage_key).await?);
    assert!(!fixture.session_exists(&clean.storage_key).await?);
    assert!(!fixture.stream_exists(&poisoned.storage_key).await?);
    assert!(fixture.session_exists(&poisoned.storage_key).await?);
    assert!(fixture.epoch_hour_marker_absent().await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_delete_expired_sessions_validation_precedes_connection() -> Result<()> {
    require_nextest();
    // No broker is spawned: the named cluster points at a dead port. Every
    // failing case below must fail with its validation message, proving the
    // CLI never connected.
    let fixture = CliFixture::offline("nats://127.0.0.1:1")?;

    let output = fixture.delete_expired(&[])?;
    assert_failure(&output);
    assert!(stdout(&output).is_empty());
    let err = stderr(&output);
    assert!(
        err.contains("cleanup_remote_sessions_days") && err.contains("unset"),
        "missing retention error: {err}"
    );

    let output = fixture.delete_expired(&["--days", "0"])?;
    assert_failure(&output);
    assert!(
        stderr(&output).contains("--days must be positive"),
        "zero days error: {}",
        stderr(&output)
    );

    let output = fixture.delete_expired(&["--days", "7", "--max", "0"])?;
    assert_failure(&output);
    assert!(
        stderr(&output).contains("--max must be positive"),
        "zero max error: {}",
        stderr(&output)
    );

    // With valid options the CLI does try to connect, and the dead endpoint
    // surfaces as a connection error instead — days resolution passed.
    let output = fixture.delete_expired(&["--days", "7"])?;
    assert_failure(&output);
    assert!(
        !stderr(&output).contains("--days"),
        "valid days must not produce a days error: {}",
        stderr(&output)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_delete_expired_sessions_disabled_retention_config_fails_clearly() -> Result<()> {
    let Some(fixture) = broker_fixture().await? else {
        return Ok(());
    };
    fixture.set_main_config("cleanup_remote_sessions_days: 0\n");
    let stale = secs_ago(AGE_STALE_SECS);
    seed_session(fixture.js(), "alpha", "old-1", stale, true).await?;

    let output = fixture.delete_expired(&[])?;
    assert_failure(&output);
    let err = stderr(&output);
    assert!(
        err.contains("cleanup_remote_sessions_days") && err.contains("disabled"),
        "disabled retention error: {err}"
    );
    assert_eq!(fixture.metadata_count().await?, 1);

    // An explicit --days overrides the disabled config for this pass only;
    // the stored config file is untouched.
    let output = fixture.delete_expired(&["--days", "30"])?;
    assert_success(&output);
    assert!(
        stdout(&output).contains("deleted: 1"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("config/config.yaml"))?,
        "cleanup_remote_sessions_days: 0\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_delete_expired_sessions_gc_lease_contention_fails_with_retry_message() -> Result<()> {
    let Some(fixture) = broker_fixture().await? else {
        return Ok(());
    };
    let stale = secs_ago(AGE_STALE_SECS);
    let alpha = seed_sessions(&fixture, "alpha", &["old-1"], stale, true).await?;

    // The test process holds the GC lease the way a concurrent worker pass
    // would; the CLI must fail clearly, not wait and not report all-zero
    // success.
    let holder = acquire_session_gc_lease(&fixture.config, CLUSTER)
        .await?
        .expect("test holds the GC lease");
    let output = fixture.delete_expired(&["--days", "30"])?;
    assert_failure(&output);
    assert!(stdout(&output).is_empty());
    let err = stderr(&output);
    assert!(
        err.contains("GC lease") && err.contains("retry"),
        "contention error: {err}"
    );
    assert!(fixture.session_exists(&alpha[0].storage_key).await?);
    holder.release().await?;

    // With the lease released the same pass succeeds.
    let output = fixture.delete_expired(&["--days", "30"])?;
    assert_success(&output);
    assert!(
        stdout(&output).contains("deleted: 1"),
        "{}",
        stdout(&output)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_delete_expired_sessions_max_bounds_admissions_and_agent_filter_scopes() -> Result<()> {
    let Some(fixture) = broker_fixture().await? else {
        return Ok(());
    };
    let stale = secs_ago(AGE_STALE_SECS);
    let alpha = seed_sessions(&fixture, "alpha", &["old-1", "old-2"], stale, true).await?;
    let beta_1 = seed_session(fixture.js(), "beta", "old-3", stale, true).await?;

    // --max 1 admits exactly one deletion; the rest wait for a later pass.
    let output = fixture.delete_expired(&["--days", "30", "--max", "1"])?;
    assert_success(&output);
    let rendered = stdout(&output);
    assert!(rendered.contains("attempted: 1, deleted: 1"), "{rendered}");
    assert!(rendered.contains("bound reached: yes"), "{rendered}");
    assert!(
        rendered.contains("The admission cap stopped this pass"),
        "{rendered}"
    );
    assert_eq!(fixture.metadata_count().await?, 2);

    // The agent filter scopes the remaining pass to alpha only.
    let output = fixture.delete_expired(&["--days", "30", "--agent", "alpha"])?;
    assert_success(&output);
    let rendered = stdout(&output);
    assert!(
        rendered.contains("agent filter=alpha") && rendered.contains("scanned: 1"),
        "{rendered}"
    );
    assert!(!fixture.session_exists(&alpha[0].storage_key).await?);
    assert!(!fixture.session_exists(&alpha[1].storage_key).await?);
    // beta was out of scope and is untouched.
    assert!(fixture.session_exists(&beta_1.storage_key).await?);
    Ok(())
}

#[test]
fn cli_info_sessions_help_mentions_scope_flags() {
    let output = run_harnx_help(&["info", "sessions"]);
    assert!(output.contains("--cluster"), "{output}");
    assert!(output.contains("--agent"), "{output}");
    assert!(output.contains("--json"), "{output}");
    assert!(
        output.contains("requires --agent") || output.contains("[--agent <AGENT>] <SESSION_ID>"),
        "session-id-requires-agent must be visible: {output}"
    );
}

#[test]
fn cli_delete_expired_sessions_help_mentions_cleanup_flags() {
    let output = run_harnx_help(&["delete", "expired-sessions"]);
    assert!(output.contains("--days"), "{output}");
    assert!(output.contains("--agent"), "{output}");
    assert!(output.contains("--max"), "{output}");
    assert!(output.contains("--dry-run"), "{output}");
    assert!(
        output.contains(&DEFAULT_MANUAL_CLEANUP_MAX.to_string()),
        "help must show the default max: {output}"
    );
    assert!(
        output.contains("cleanup_remote_sessions_days"),
        "help must explain days resolution: {output}"
    );
}

// qual:test_helper
fn run_harnx_help(args: &[&str]) -> String {
    let mut all = args.to_vec();
    all.push("--help");
    let output = run_harnx(std::path::Path::new("/tmp/harnx-cli-help-home"), &all, &[])
        .expect("run harnx --help");
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status.code(),
        stderr(&output)
    );
    stdout(&output)
}
