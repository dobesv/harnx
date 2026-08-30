//! Unit tests for nats_worker module.

use crate::config::remote_session_ops::{
    delete_remote_message_range, edit_remote_message_range, load_remote_session_for_render,
    load_remote_transcript_for_render, rewind_remote_session,
};
use crate::config::{self, Config};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{
    activity_key, SessionActivity, SessionAgentSource, SessionMetadata, SessionMetadataStore,
};
use crate::nats_worker::agent_loop::tool_can_rerun;
use crate::nats_worker::run_worker_daemon;
use crate::NatsSession;
use anyhow::{bail, Context};
use futures_util::StreamExt;
use harnx_core::agent_config::AgentConfig;
use harnx_core::event::{AgentEvent, AgentEventSink};
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::session_reconstruct::{reconstruct_state_from_nats, TurnStatus};
use harnx_core::tool::{ToolCall, ToolDeclaration, ToolProvider};
use harnx_toolset::Toolset;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

mod subagent_discovery_tests;
mod transcript_render_tests;

/// Spawn a local JetStream-enabled nats-server on a free port with an isolated
/// temp store dir, returning the connect URL, the child process, and the temp
/// dir guard. Using a free port + per-run store dir avoids cross-run state
/// bleed (JetStream KV/lease buckets) and port collisions that make tests flaky
/// when run repeatedly or in parallel. Returns `None` if nats-server is absent.
pub(crate) async fn spawn_test_nats() -> Option<(String, std::process::Child, tempfile::TempDir)> {
    if which::which("nats-server").is_err() {
        eprintln!("skipping: nats-server not available");
        return None;
    }
    // Bind an ephemeral port, then release it for nats-server to claim.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        listener.local_addr().ok()?.port()
    };
    let store_dir = tempfile::tempdir().ok()?;
    let child = std::process::Command::new("nats-server")
        .args(["-js", "-sd"])
        .arg(store_dir.path())
        .args(["-p", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let url = format!("nats://127.0.0.1:{port}");
    // Poll for readiness rather than a fixed sleep.
    for _ in 0..50 {
        if async_nats::connect(&url).await.is_ok() {
            return Some((url, child, store_dir));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let mut child = child;
    let _ = child.kill();
    let _ = child.wait();
    None
}

const NATS_TEST_CONDITION_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) use crate::test_environment::{env_lock_async as env_lock, EnvGuard as TestEnvGuard};

fn load_config_via_internal_pipeline(config_path: &Path) -> Config {
    let config_dir = config_path
        .parent()
        .expect("config path must have parent directory");
    let _config_guard = TestEnvGuard::new("HARNX_CONFIG_DIR", config_dir);
    Config::load_from_file(config_path).unwrap()
}

pub(super) struct SeededRemoteParentConfig {
    temp: tempfile::TempDir,
    pub(super) parent_config: Config,
}

impl SeededRemoteParentConfig {
    pub(super) fn config_dir(&self) -> &Path {
        self.temp.path()
    }
}

fn expected_metis_remote_tool_names() -> Vec<String> {
    vec!["metis__at__local_session_handoff".to_string()]
}

pub(super) fn seed_remote_config(url: &str) -> SeededRemoteParentConfig {
    use std::fs;

    let temp = tempfile::TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("nats_servers")).unwrap();
    fs::create_dir_all(temp.path().join("agents")).unwrap();
    fs::write(
        temp.path().join("nats_servers/local.yaml"),
        format!(
            "url: {url}
agents:
  - name: metis
    description: Remote planner over NATS
"
        ),
    )
    .unwrap();
    fs::write(
        temp.path().join("agents/metis.md"),
        "---\nmodel: test:test-model\nuse_tools: \"*\"\n---\nstub worker prompt\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("config.yaml"),
        "model: test:test-model
use_tools:
  - metis@local
",
    )
    .unwrap();

    SeededRemoteParentConfig {
        parent_config: load_config_via_internal_pipeline(&temp.path().join("config.yaml")),
        temp,
    }
}

fn assert_remote_tool_family(parent_config: &mut Config) {
    let expected_tools = expected_metis_remote_tool_names();

    let mut whitelisted_names: Vec<String> = parent_config
        .tool_declarations_for_use_tools(Some("metis@local"), None)
        .0
        .into_iter()
        .map(|tool| tool.name)
        .filter(|name| name.starts_with("metis__at__local_session_"))
        .collect();
    whitelisted_names.sort();
    assert_eq!(whitelisted_names, expected_tools);
}

fn assert_remote_tool_descriptions(parent_config: &mut Config) {
    let wildcard_tools = parent_config
        .tool_declarations_for_use_tools(Some("*"), None)
        .0;
    let mut wildcard_names: Vec<String> = wildcard_tools
        .iter()
        .map(|tool| tool.name.clone())
        .filter(|name| name.starts_with("metis__at__local_session_"))
        .collect();
    wildcard_names.sort();
    assert_eq!(wildcard_names, expected_metis_remote_tool_names());

    let handoff_tool = wildcard_tools
        .iter()
        .find(|tool| tool.name == "metis__at__local_session_handoff")
        .expect("handoff tool must exist");
    assert!(handoff_tool
        .description
        .contains("Remote planner over NATS"));
}

async fn seed_remote_dispatch_session_log(
    log: &NatsSessionLog,
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> anyhow::Result<()> {
    use harnx_core::message::{MessageContent, MessageRole};

    let metadata_store = SessionMetadataStore::ensure(jetstream, 1).await?;
    metadata_store
        .create(&SessionMetadata::new(
            session_id,
            crate::SessionInitializer::named("metis", Default::default()),
        ))
        .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("first prompt".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text("first reply".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("second prompt".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    Ok(())
}

fn remote_dispatch_user_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    harnx_core::session_reconstruct::apply_log_mutations_nats(entries)
        .expect("reconstruct remote dispatch effective log")
        .into_iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { role, content, .. } if role.is_user() => {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect()
}

fn spawn_metis_worker(url: &str) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    spawn_metis_worker_with_call_fn(url, fixed_prompt_call_fn("stub remote reply over nats"))
}

/// Like [`spawn_metis_worker`] but with a lease `renew_interval` short enough
/// that at least one renewal fires during a session's brief active window.
///
/// The daemon releases the session lease as soon as the turn goes idle
/// (`execute_session` breaks and calls `lease.release()` within a second or
/// two of a fast stub turn). The default 10s renew interval never ticks in
/// that window, so a test asserting that lease renewal refreshes the session
/// index would time out. A sub-second interval makes the renewal — and its
/// index `last_activity` refresh — fire deterministically while the session is
/// held active.
///
/// A worker call_fn that holds the turn open for `delay` before replying, so
/// the session stays active (lease held, renew task running) long enough for a
/// lease renewal — and its session-index `last_activity` refresh — to fire.
fn slow_prompt_call_fn(reply: &'static str, delay: Duration) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok((
                reply.to_string(),
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

fn spawn_metis_worker_with_fast_renew(
    url: &str,
    call_fn: crate::agent_loop::AgentCallFn,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let mut daemon = crate::nats_worker::WorkerDaemonConfig::managing("local", "worker-metis");
    // ttl must stay > renew_interval; keep a comfortable margin.
    daemon.lease.renew_interval = Duration::from_millis(300);
    daemon.lease.ttl = Duration::from_secs(5);
    spawn_metis_worker_with_call_fn_and_daemon(url, call_fn, daemon)
}

fn spawn_metis_worker_with_call_fn(
    url: &str,
    call_fn: crate::agent_loop::AgentCallFn,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    spawn_metis_worker_with_call_fn_and_daemon(
        url,
        call_fn,
        crate::nats_worker::WorkerDaemonConfig::managing("local", "worker-metis"),
    )
}

fn spawn_metis_worker_with_call_fn_and_daemon(
    url: &str,
    call_fn: crate::agent_loop::AgentCallFn,
    daemon: crate::nats_worker::WorkerDaemonConfig,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    spawn_metis_worker_with_hooks(url, call_fn, daemon, None)
}

pub(super) fn spawn_metis_worker_with_hooks(
    url: &str,
    call_fn: crate::agent_loop::AgentCallFn,
    daemon: crate::nats_worker::WorkerDaemonConfig,
    hooks: Option<harnx_core::hooks::HooksConfig>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let mut worker_agent =
        AgentConfig::from_markdown("metis", "---\nuse_tools: \"*\"\n---\nstub worker prompt")
            .unwrap();
    // A real worker starts with its model already resolved. Without this the
    // fixture has no client for `test:`, so loading an agent file that declares
    // `model: test:test-model` fails outright.
    worker_agent.set_resolved_model(harnx_core::model::Model::new("test", "test-model"));
    let worker_config = Config {
        data: harnx_core::config_data::ConfigData {
            model_id: "test:test-model".to_string(),
            hooks,
            ..Default::default()
        },
        agent: Some(crate::config::Agent::new(worker_agent)),
        nats_servers: vec![config::NatsServerConfig {
            name: "local".to_string(),
            url: url.to_string(),
            token: None,
            replicas: None,
            tls: Some(false),
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: Vec::new(),
        }],
        ..Default::default()
    };
    let worker_config = Arc::new(parking_lot::RwLock::new(worker_config));
    tokio::spawn({
        let worker_config = Arc::clone(&worker_config);
        async move { run_worker_daemon(worker_config, daemon, Some(call_fn)).await }
    })
}

async fn run_remote_round_trip(parent_config: Config) -> anyhow::Result<()> {
    run_remote_round_trip_with_session_id(
        parent_config,
        crate::nats_worker::new_remote_session_id(),
    )
    .await
}

async fn run_remote_round_trip_with_session_id(
    parent_config: Config,
    session_id: String,
) -> anyhow::Result<()> {
    run_remote_round_trip_with_session_id_and_sink(
        parent_config,
        session_id,
        Arc::new(NoopEventSink),
        "local",
    )
    .await
}

fn cluster_shared_session_config(
    cluster: impl Into<String>,
    session_id: impl Into<String>,
) -> crate::NatsSessionConfig {
    crate::NatsSessionConfig {
        cluster: cluster.into(),
        initializer: crate::SessionInitializer::named("metis", Default::default()),
        session_id: Some(session_id.into()),
        activation_route: crate::SessionActivationRoute::ClusterShared,
    }
}

pub(super) async fn run_remote_round_trip_with_session_id_and_sink(
    parent_config: Config,
    session_id: String,
    sink: Arc<dyn AgentEventSink>,
    cluster: &str,
) -> anyhow::Result<()> {
    subagent_discovery_tests::wait_for_cluster_worker(&parent_config, cluster).await?;

    let mut parent_config = parent_config;
    let parent_session =
        crate::config::session::new(&parent_config, "parent-nats-roundtrip", None)?;
    parent_config.session = Some(parent_session);
    let parent_global_config = Arc::new(parking_lot::RwLock::new(parent_config));
    let abort_signal = harnx_core::abort::create_abort_signal();
    let session_cfg = cluster_shared_session_config(cluster, session_id);
    let session =
        crate::NatsSession::from_global_config(session_cfg, &parent_global_config, abort_signal)
            .await?;

    const REMOTE_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(60);
    let turn_result = tokio::time::timeout(
        REMOTE_ROUND_TRIP_TIMEOUT,
        session.run_turn("delegate over nats", sink, None),
    )
    .await
    .with_context(|| {
        format!(
            "run_turn timed out after {}s in remote NATS round-trip test",
            REMOTE_ROUND_TRIP_TIMEOUT.as_secs()
        )
    })??;
    let reply = turn_result.response.with_context(|| {
        format!(
            "NATS session turn must return final assistant response (error={:?}, cancelled={}, user_seq={})",
            turn_result.error, turn_result.was_cancelled, turn_result.user_msg_seq
        )
    })?;
    anyhow::ensure!(
        reply.contains("stub remote reply over nats"),
        "expected reply to contain stub remote reply, got: {reply}"
    );
    Ok(())
}

#[test]
fn metrics_snapshot_tracks_counters() {
    crate::nats_metrics::reset_for_test();
    crate::nats_metrics::active_session_started();
    crate::nats_metrics::lease_acquired();
    crate::nats_metrics::lease_lost();
    crate::nats_metrics::fenced_write_rejected();
    crate::nats_metrics::resume_detected();
    crate::nats_metrics::interrupt_error_synthesized();

    let snapshot = crate::nats_metrics::snapshot();
    assert_eq!(snapshot.active_sessions_per_worker, 1);
    assert_eq!(snapshot.lease_acquisitions, 1);
    assert_eq!(snapshot.lease_losses, 1);
    assert_eq!(snapshot.fenced_writes_rejected, 1);
    assert_eq!(snapshot.resumes, 1);
    assert_eq!(snapshot.interrupt_errors_synthesized, 1);

    crate::nats_metrics::active_session_finished();
    let snapshot = crate::nats_metrics::snapshot();
    assert_eq!(snapshot.active_sessions_per_worker, 0);
}

/// Unit test for the partitioning logic: idempotent tools get re-run,
/// non-idempotent tools get synthesized error.
#[test]
fn test_orphan_partition_logic() {
    use std::collections::HashMap;

    // Build decl_map with idempotent "echo" and non-idempotent "write_file"
    let mut decl_map: HashMap<String, ToolDeclaration> = HashMap::new();
    decl_map.insert(
        "echo".to_string(),
        ToolDeclaration {
            name: "echo".to_string(),
            description: "Echo tool".to_string(),
            parameters: harnx_core::tool::JsonSchema::default(),
            mcp_tool_name: None,
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: Some(true),
            read_only_hint: Some(true),
        },
    );
    decl_map.insert(
        "write_file".to_string(),
        ToolDeclaration {
            name: "write_file".to_string(),
            description: "Write tool".to_string(),
            parameters: harnx_core::tool::JsonSchema::default(),
            mcp_tool_name: None,
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
        },
    );

    // Simulate orphan calls
    let calls = vec![
        ToolCall {
            name: "echo".to_string(),
            arguments: json!({"message": "hello"}),
            id: Some("call_echo_1".to_string()),
            thought_signature: None,
        },
        ToolCall {
            name: "write_file".to_string(),
            arguments: json!({"path": "/tmp/test", "content": "data"}),
            id: Some("call_write_1".to_string()),
            thought_signature: None,
        },
    ];

    // Partition based on hints
    let mut rerun_calls: Vec<ToolCall> = Vec::new();
    let mut synthesize_count = 0;

    for call in &calls {
        // Exercise the production decision function, not a re-implementation.
        if tool_can_rerun(&decl_map, &call.name) {
            rerun_calls.push(call.clone());
        } else {
            synthesize_count += 1;
        }
    }

    // Assertions
    assert_eq!(rerun_calls.len(), 1, "echo should be marked for re-run");
    assert_eq!(rerun_calls[0].name, "echo");
    assert_eq!(synthesize_count, 1, "write_file should be synthesized");

    // Unknown tools must default to non-rerunnable (synthesize).
    assert!(
        !tool_can_rerun(&decl_map, "nonexistent_tool"),
        "unknown tools must not be re-run"
    );
}

/// Verify that the ToolOutput for non-idempotent tools contains the expected error message.
#[test]
fn test_non_idempotent_output_is_synthesized() {
    let lost = ToolOutput {
        id: Some("call_1".to_string()),
        name: "write_file".to_string(),
        output: json!({
            "error": "tool response lost (session was interrupted before results were persisted)"
        }),
        markdown: None,
        content: Vec::new(),
        switch_agent: None,
    };

    let output_str = lost.output.to_string();
    assert!(
        output_str.contains("tool response lost"),
        "expected synthesized error message, got: {output_str}"
    );
    assert!(
        !output_str.contains("marked for re-run"),
        "should NOT contain 'marked for re-run' placeholder"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn control_log_append_requires_live_lease() {
    use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
    use crate::nats_worker::daemon::should_append_control_log_entry;

    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let client = async_nats::connect(&url).await.unwrap();
    let jetstream = async_nats::jetstream::new(client);

    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream,
        session_id: "control-lease-gate",
        worker_id: "worker-a".to_string(),
        generation: 1,
        config: NatsLeaseConfig {
            ttl: std::time::Duration::from_secs(5),
            renew_interval: std::time::Duration::from_millis(500),
            replicas: 1,
            tombstone_ttl: std::time::Duration::from_secs(10),
            ..Default::default()
        },
        session_metadata: None,
    })
    .await
    .unwrap()
    .expect("lease should be acquired with no contention");

    // While the lease is held, control-command log appends are permitted.
    assert!(
        should_append_control_log_entry(&lease),
        "held lease must allow control log appends"
    );

    // After the lease is lost (failover / fenced out), appends must be skipped.
    lease.mark_lost_for_test();
    assert!(
        !should_append_control_log_entry(&lease),
        "lost lease must skip control log appends"
    );

    // Regardless of lease state, the Cancel path must still be able to abort.
    let abort_signal = harnx_core::abort::create_abort_signal();
    abort_signal.set_ctrlc();
    assert!(
        abort_signal.aborted(),
        "cancel path still aborts worker without lease"
    );

    let _ = child.kill();
    let _ = child.wait();
}

pub(super) struct NoopEventSink;

impl AgentEventSink for NoopEventSink {
    fn emit(&self, _event: AgentEvent) {}
}

pub(super) fn fixed_prompt_call_fn(reply: &'static str) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        Box::pin(async move {
            Ok((
                reply.to_string(),
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

fn echoing_call_fn(captured: Arc<AsyncMutex<Vec<String>>>) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        let captured = Arc::clone(&captured);
        let derived = input.text();
        Box::pin(async move {
            captured.lock().await.push(derived.clone());
            Ok((
                format!("stub remote reply over nats: {derived}"),
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn test_subagent_toolset(
    url: &str,
    timeouts: super::subagent_toolset::SubagentTimeouts,
) -> Arc<super::subagent_toolset::SubagentToolset> {
    let client = async_nats::connect(url)
        .await
        .expect("connect sub-agent toolset to test nats");
    Arc::new(super::subagent_toolset::SubagentToolset::with_timeouts(
        "metis",
        super::subagent_toolset::SubagentSessionRoute::new(
            "local",
            crate::SessionActivationRoute::ClusterShared,
        ),
        client.clone(),
        async_nats::jetstream::new(client),
        timeouts,
    ))
}

fn subagent_test_env<'a>(
    url: &'a str,
    seeded: &'a SeededRemoteParentConfig,
) -> (TestEnvGuard, TestEnvGuard, TestEnvGuard) {
    (
        TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir()),
        TestEnvGuard::new("HARNX_NATS_URL", url),
        TestEnvGuard::new("HARNX_NATS_TOKEN", "test-token"),
    )
}

async fn load_effective_entries(log: &NatsSessionLog) -> Vec<(u64, SessionLogEntry)> {
    let entries = log
        .load_events_async()
        .await
        .expect("load session log entries");
    harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)
        .expect("reconstruct effective session log")
}

pub(super) async fn wait_for_condition<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_cancel_published_after_in_flight_marks_session_cancelled() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    assert_remote_tool_family(&mut seeded.parent_config);
    assert_remote_tool_descriptions(&mut seeded.parent_config);

    let session_id = crate::nats_worker::new_remote_session_id();
    let entered = Arc::new(Notify::new());
    let worker_saw_abort = Arc::new(AtomicBool::new(false));
    let release_after_assertion = Arc::new(Notify::new());
    let call_fn: crate::agent_loop::AgentCallFn = {
        let entered = Arc::clone(&entered);
        let worker_saw_abort = Arc::clone(&worker_saw_abort);
        let release_after_assertion = Arc::clone(&release_after_assertion);
        Arc::new(move |_input, _config, abort| {
            let entered = Arc::clone(&entered);
            let worker_saw_abort = Arc::clone(&worker_saw_abort);
            let release_after_assertion = Arc::clone(&release_after_assertion);
            Box::pin(async move {
                entered.notify_one();
                tokio::select! {
                    _ = harnx_core::abort::wait_abort_signal(&abort) => {
                        worker_saw_abort.store(true, Ordering::SeqCst);
                        release_after_assertion.notified().await;
                        bail!("worker call_fn interrupted after remote cancel")
                    }
                    _ = tokio::time::sleep(Duration::from_secs(20)) => {
                        bail!("worker call_fn timed out waiting for remote cancel abort")
                    }
                }
            })
        })
    };

    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);

    let mut parent_config = seeded.parent_config;
    let parent_session =
        crate::config::session::new(&parent_config, "parent-nats-remote-cancel", None)
            .expect("create parent session");
    parent_config.session = Some(parent_session);
    let parent_global_config = Arc::new(parking_lot::RwLock::new(parent_config));
    let abort_signal = harnx_core::abort::create_abort_signal();
    let session_cfg = cluster_shared_session_config("local", session_id.clone());
    let session =
        crate::NatsSession::from_global_config(session_cfg, &parent_global_config, abort_signal)
            .await
            .expect("build NATS session");

    let run_turn = tokio::spawn(async move {
        session
            .run_turn("delegate over nats", Arc::new(NoopEventSink), None)
            .await
    });

    tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, entered.notified())
        .await
        .expect("worker never entered in-flight call_fn before cancel publish");

    let raw_client = async_nats::connect(&url)
        .await
        .expect("connect raw nats client for cancel publish");
    crate::send_control_command(&raw_client, &session_id, crate::ControlCommand::Cancel)
        .await
        .expect("publish cancel control command");

    assert!(
        wait_for_condition(NATS_TEST_CONDITION_TIMEOUT, || worker_saw_abort
            .load(Ordering::SeqCst))
        .await,
        "worker call_fn never observed abort after remote cancel publish"
    );

    let log_client = async_nats::connect(&url)
        .await
        .expect("connect nats client for session log polling");
    let log = NatsSessionLog::new(async_nats::jetstream::new(log_client), session_id.clone());
    let _cancel_entries = tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, async {
        loop {
            let entries = log
                .load_events_async()
                .await
                .expect("load session log entries");
            if entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. }))
            {
                break entries;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("durable session log never recorded Cancel entry");

    let _turn_result = tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, run_turn)
        .await
        .expect("run_turn task did not complete after remote cancel")
        .expect("run_turn join must succeed")
        .expect("run_turn must return result after remote cancel");

    let final_entries = log
        .load_events_async()
        .await
        .expect("reload final session log entries after run_turn completion");
    let cancel_fence_token = final_entries.iter().find_map(|(_, entry)| match entry {
        SessionLogEntry::Cancel { fence_token } => Some(*fence_token),
        _ => None,
    });
    let first_assistant_fence_token = final_entries.iter().find_map(|(_, entry)| match entry {
        SessionLogEntry::Message {
            role, fence_token, ..
        } if role.is_assistant() => *fence_token,
        _ => None,
    });
    let reconstructed = reconstruct_state_from_nats(&final_entries);
    assert_eq!(
        reconstructed.turn_status,
        TurnStatus::InFlightCancelled,
        "durable log should reconstruct to InFlightCancelled after remote cancel when no assistant message follows cancel; cancel fence={cancel_fence_token:?}, assistant fence={first_assistant_fence_token:?}, entries={final_entries:#?}"
    );
    release_after_assertion.notify_one();
    assert!(
        wait_for_condition(NATS_TEST_CONDITION_TIMEOUT, || {
            crate::nats_metrics::snapshot().active_sessions_per_worker == 0
        })
        .await,
        "worker session task did not finish after remote cancel"
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = child.kill();
    let _ = child.wait();
}

async fn registered_agent_provider(
    jetstream: &async_nats::jetstream::Context,
    config: &Config,
    agents: &[&str],
    active_package: Option<&str>,
) -> (
    String,
    Arc<crate::nats_tool_provider::NatsToolProvider>,
    Vec<(String, harnx_toolset::Registration)>,
) {
    let (instance_id, registrations) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(registry) = jetstream
                .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
                .await
            {
                let mut keys = registry.keys().await.expect("list registry keys");
                let mut registrations = Vec::new();
                while let Some(key) = keys.next().await {
                    let key = key.expect("read registry key");
                    let Some(value) = registry.get(&key).await.expect("read registration") else {
                        continue;
                    };
                    let registration: harnx_toolset::Registration =
                        serde_json::from_slice(&value).expect("decode registration");
                    registrations.push((key, registration));
                }
                let registration_agent = |registration: &harnx_toolset::Registration| {
                    registration.package.as_ref().map_or_else(
                        || registration.server.clone(),
                        |package| format!("{package}/{}", registration.server),
                    )
                };
                if agents.iter().all(|agent| {
                    registrations
                        .iter()
                        .any(|(_, registration)| registration_agent(registration) == *agent)
                }) {
                    let agent = agents.first().expect("at least one requested agent");
                    let (key, registration) = registrations
                        .iter()
                        .find(|(_, registration)| registration_agent(registration) == *agent)
                        .expect("requested agent registration exists");
                    let identity =
                        crate::server_identity::ServerIdentity::identity_token(registration);
                    let instance_id = key
                        .strip_suffix(&format!(".{identity}"))
                        .expect("registry key uses {instance}.{identity_token}")
                        .to_string();
                    break (instance_id, registrations);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("worker did not register configured agents");
    let provider = crate::nats_tool_provider::NatsToolProvider::discover(
        config,
        harnx_core::instance::ServerScope::from_string(instance_id.clone()),
        crate::nats_tool_provider::NatsInFlightCalls::default(),
        active_package,
    )
    .await
    .expect("parent discovers configured sub-agent toolsets");
    (instance_id, Arc::new(provider), registrations)
}

async fn call_registered_agent(
    provider: Arc<crate::nats_tool_provider::NatsToolProvider>,
    tool: String,
    message: String,
    early_event: Option<(&mut async_nats::Subscriber, &str)>,
) -> (serde_json::Value, Option<String>) {
    let prompt_call = tokio::spawn(async move {
        provider
            .call_tool(
                &tool,
                json!({ "message": message }),
                &harnx_core::abort::create_abort_signal(),
            )
            .await
    });
    let child_session_id = if let Some((parent_events, expected_agent)) = early_event {
        let (agent, session_id) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let message = parent_events
                    .next()
                    .await
                    .expect("parent event stream closed");
                let envelope =
                    crate::nats_event_sink::AdvisoryEnvelope::from_bytes(&message.payload)
                        .expect("decode parent advisory");
                if let AgentEvent::SubAgent { source, event } = envelope.event {
                    if let AgentEvent::Turn(harnx_core::event::TurnEvent::SubAgentStarted {
                        agent,
                        session_id,
                    }) = *event
                    {
                        assert_eq!(source.agent, agent);
                        assert_eq!(source.session_id.as_deref(), Some(session_id.as_str()));
                        break (agent, session_id);
                    }
                }
            }
        })
        .await
        .expect("parent did not receive early SubAgentStarted");
        assert_eq!(agent, expected_agent);
        assert!(
            !prompt_call.is_finished(),
            "SubAgentStarted must arrive before final tool result"
        );
        Some(session_id)
    } else {
        None
    };
    let result = prompt_call
        .await
        .expect("join prompt tool call")
        .unwrap_or_else(|error| match error {
            harnx_core::tool::ToolError::Recoverable(error)
            | harnx_core::tool::ToolError::Fatal(error) => {
                panic!("registered agent prompt failed: {error:#}")
            }
        });
    (result.value, child_session_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_registers_and_delegates_to_every_configured_agent() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let agents_dir = seeded.config_dir().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("create configured agents directory");
    for agent in ["alpha", "beta", "metis"] {
        std::fs::write(
            agents_dir.join(format!("{agent}.md")),
            "---\nmodel: test:test-model\n---\nConfigured sub-agent\n",
        )
        .expect("write configured agent");
    }

    let captured = Arc::new(AsyncMutex::new(Vec::new()));
    let daemon = spawn_metis_worker_with_call_fn(&url, echoing_call_fn(Arc::clone(&captured)));
    let client = async_nats::connect(&url)
        .await
        .expect("connect registry observer");
    let jetstream = async_nats::jetstream::new(client);
    let (instance_id, provider, registrations) = registered_agent_provider(
        &jetstream,
        &seeded.parent_config,
        &["alpha", "beta", "metis"],
        None,
    )
    .await;

    // Old auto-registration included the active agent; preserve self-delegation parity.
    for agent in ["alpha", "beta", "metis"] {
        let (key, registration) = registrations
            .iter()
            .find(|(_, registration)| registration.server == agent)
            .expect("configured agent registration exists");
        assert_eq!(key, &format!("{instance_id}.____{agent}"));
        assert_eq!(registration.tools.len(), 4);
        assert!(registration
            .tools
            .iter()
            .all(|tool| tool.name.starts_with("session_")));
    }

    let declarations = provider.declarations_for_use_tools(Some("*"));
    for agent in ["alpha", "beta"] {
        assert!(declarations
            .iter()
            .any(|tool| tool.name == format!("{agent}_session_prompt")));
        let (result, _) = call_registered_agent(
            Arc::clone(&provider),
            format!("{agent}_session_prompt"),
            format!("delegate to {agent}"),
            None,
        )
        .await;
        assert_eq!(
            result["response"],
            format!("stub remote reply over nats: delegate to {agent}")
        );
    }

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_new_and_prompt_results_include_agent_source_marker() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let daemon = spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("marker response"));
    let toolset = test_subagent_toolset(
        &url,
        super::subagent_toolset::SubagentTimeouts::new(
            Duration::from_secs(2),
            Duration::from_secs(10),
        ),
    )
    .await;

    let created = toolset
        .invoke("session_new", json!({}), CancellationToken::new())
        .await
        .expect("create marked child session");
    let session_id = created["session_id"]
        .as_str()
        .expect("new result session id")
        .to_string();
    assert_eq!(created["sub_agent"]["agent"], "metis");
    assert_eq!(created["sub_agent"]["session_id"], session_id);
    assert!(created["sub_agent"].get("model").is_none());

    let prompted = toolset
        .invoke(
            "session_prompt",
            json!({ "message": "continue marked session", "session_id": session_id }),
            CancellationToken::new(),
        )
        .await
        .expect("prompt marked child session");
    assert_eq!(prompted["sub_agent"]["agent"], "metis");
    assert_eq!(prompted["sub_agent"]["session_id"], session_id);
    assert_eq!(prompted["session_id"], session_id);

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_started_reaches_parent_stream_before_prompt_result() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let mut seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let agents_dir = seeded.config_dir().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("create agents directory");
    std::fs::write(
        agents_dir.join("metis.md"),
        "---\nmodel: test:test-model\n---\nConfigured metis agent\n",
    )
    .expect("write metis agent");
    let parent_session =
        crate::config::session::new(&seeded.parent_config, "parent-subagent-start", None)
            .expect("create parent session");
    let parent_session_id = parent_session.id().to_string();
    seeded.parent_config.session = Some(parent_session);

    let daemon = spawn_metis_worker_with_call_fn(
        &url,
        slow_prompt_call_fn("early event child response", Duration::from_millis(300)),
    );
    let client = async_nats::connect(&url)
        .await
        .expect("connect parent event observer");
    let mut parent_events = client
        .subscribe(crate::nats_event_sink::events_subject(&parent_session_id))
        .await
        .expect("subscribe parent event stream");
    client
        .flush()
        .await
        .expect("flush parent event subscription");
    let jetstream = async_nats::jetstream::new(client);
    let (_, provider, _) =
        registered_agent_provider(&jetstream, &seeded.parent_config, &["metis"], None).await;
    let (result, child_session_id) = call_registered_agent(
        provider,
        "metis_session_prompt".to_string(),
        "emit start before finishing".to_string(),
        Some((&mut parent_events, "metis")),
    )
    .await;
    let child_session_id = child_session_id.expect("SubAgentStarted includes child session id");
    assert_eq!(result["response"], "early event child response");
    assert_eq!(result["sub_agent"]["agent"], "metis");
    assert_eq!(result["sub_agent"]["session_id"], child_session_id);

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_new_prompt_reuse_and_load_share_one_session_log() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let captured = Arc::new(AsyncMutex::new(Vec::new()));
    let daemon = spawn_metis_worker_with_call_fn(&url, echoing_call_fn(Arc::clone(&captured)));
    let toolset = test_subagent_toolset(
        &url,
        super::subagent_toolset::SubagentTimeouts::new(
            Duration::from_secs(2),
            Duration::from_secs(10),
        ),
    )
    .await;

    let created = toolset
        .invoke("session_new", json!({}), CancellationToken::new())
        .await
        .expect("create and initialize child session");
    let session_id = created["session_id"]
        .as_str()
        .expect("session_new returns session_id")
        .to_string();
    let log = NatsSessionLog::new(
        seeded
            .parent_config
            .nats_jetstream("local")
            .await
            .expect("child log jetstream"),
        session_id.clone(),
    );
    let after_new = log
        .load_events_async()
        .await
        .expect("load child log after session_new")
        .len();

    for (message, expected) in [
        (
            "first continuation",
            "stub remote reply over nats: first continuation",
        ),
        (
            "second continuation",
            "stub remote reply over nats: second continuation",
        ),
    ] {
        let result = toolset
            .invoke(
                "session_prompt",
                json!({ "message": message, "session_id": session_id }),
                CancellationToken::new(),
            )
            .await
            .expect("continue child session");
        assert_eq!(result["session_id"], session_id);
        assert_eq!(result["response"], expected);
    }

    let loaded = toolset
        .invoke(
            "session_load",
            json!({ "session_id": session_id }),
            CancellationToken::new(),
        )
        .await
        .expect("load child session through tool");
    let loaded_events = loaded["events"]
        .as_array()
        .expect("session_load returns serialized events");
    assert!(loaded_events.len() > after_new);
    let final_entries = log
        .load_events_async()
        .await
        .expect("load final reused child log");
    assert!(final_entries.len() > after_new);
    assert_eq!(
        captured.lock().await.as_slice(),
        [
            "Start a new session.",
            "first continuation",
            "second continuation"
        ]
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_explicit_cancel_stops_in_flight_child_turn() {
    run_subagent_cancel_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_parent_abort_stops_in_flight_child_turn() {
    run_subagent_cancel_case(true).await;
}

async fn run_subagent_cancel_case(parent_abort: bool) {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let entered = Arc::new(Notify::new());
    let entered_call = Arc::clone(&entered);
    let call_fn: crate::agent_loop::AgentCallFn = Arc::new(move |_input, _config, abort| {
        let entered = Arc::clone(&entered_call);
        Box::pin(async move {
            entered.notify_one();
            harnx_core::abort::wait_abort_signal(&abort).await;
            bail!("child call cancelled")
        })
    });
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
    let toolset = test_subagent_toolset(
        &url,
        super::subagent_toolset::SubagentTimeouts::new(
            Duration::from_secs(5),
            Duration::from_secs(10),
        ),
    )
    .await;
    let session_id = crate::nats_worker::new_remote_session_id();
    let parent_cancel = CancellationToken::new();
    let prompt = tokio::spawn({
        let toolset = Arc::clone(&toolset);
        let parent_cancel = parent_cancel.clone();
        let session_id = session_id.clone();
        async move {
            toolset
                .invoke(
                    "session_prompt",
                    json!({ "message": "block until cancelled", "session_id": session_id }),
                    parent_cancel,
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("child worker did not enter model call");

    if parent_abort {
        parent_cancel.cancel();
    } else {
        toolset
            .invoke(
                "session_cancel",
                json!({ "session_id": session_id }),
                CancellationToken::new(),
            )
            .await
            .expect("explicit child cancel tool succeeds");
    }
    let error = tokio::time::timeout(Duration::from_secs(5), prompt)
        .await
        .expect("cancelled prompt tool did not return")
        .expect("join cancelled prompt task")
        .expect_err("cancelled prompt must return an error");
    assert!(
        error.to_string().contains("cancel") || error.to_string().contains("abort"),
        "unexpected cancellation error: {error}"
    );
    let log = NatsSessionLog::new(
        seeded
            .parent_config
            .nats_jetstream("local")
            .await
            .expect("cancel log jetstream"),
        session_id,
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let entries = log.load_events_async().await.expect("load cancel entries");
            if entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("child cancel tombstone was not persisted");

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_operation_timeout_returns_error_and_cancels_child() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let call_fn = slow_prompt_call_fn("too late", Duration::from_secs(30));
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
    let toolset = test_subagent_toolset(
        &url,
        super::subagent_toolset::SubagentTimeouts::new(
            Duration::from_secs(5),
            Duration::from_millis(150),
        ),
    )
    .await;
    let session_id = crate::nats_worker::new_remote_session_id();
    let error = toolset
        .invoke(
            "session_prompt",
            json!({ "message": "time out", "session_id": session_id }),
            CancellationToken::new(),
        )
        .await
        .expect_err("operation timeout must return an error");
    assert!(error.to_string().contains("overall timeout"));
    let log = NatsSessionLog::new(
        seeded
            .parent_config
            .nats_jetstream("local")
            .await
            .expect("timeout log jetstream"),
        session_id,
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let entries = log.load_events_async().await.expect("load timeout entries");
            if entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("operation timeout did not cancel child");

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

async fn spawn_child_activity_heartbeat(
    url: &str,
    session_id: &str,
) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let client = async_nats::connect(url)
        .await
        .expect("connect activity publisher");
    let subject = crate::nats_event_sink::events_subject(session_id);
    let payload = crate::nats_event_sink::AdvisoryEnvelope::new(
        u64::MAX,
        AgentEvent::Turn(harnx_core::event::TurnEvent::Started),
    )
    .to_bytes()
    .expect("encode activity");
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = task_stop.cancelled() => break,
                () = tokio::time::sleep(Duration::from_millis(40)) => {}
            }
            client
                .publish(subject.clone(), payload.clone().into())
                .await
                .expect("publish child activity");
            client.flush().await.expect("flush child activity");
        }
    });
    (stop, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_idle_timeout_resets_on_child_activity() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let call_fn: crate::agent_loop::AgentCallFn = Arc::new(move |_input, _config, _abort| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(225)).await;
            Ok((
                "active child completed".to_string(),
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    });
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
    let toolset = test_subagent_toolset(
        &url,
        super::subagent_toolset::SubagentTimeouts::new(
            Duration::from_millis(75),
            Duration::from_secs(10),
        ),
    )
    .await;
    let session_id = crate::nats_worker::new_remote_session_id();
    let (activity_stop, activity_publisher) =
        spawn_child_activity_heartbeat(&url, &session_id).await;
    let result = toolset
        .invoke(
            "session_prompt",
            json!({ "message": "stay active", "session_id": session_id }),
            CancellationToken::new(),
        )
        .await;
    activity_stop.cancel();
    activity_publisher.await.expect("join activity publisher");
    let result = result.expect("activity must keep idle timeout from false-firing");
    assert_eq!(result["response"], "active child completed");

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_subagent_prompt_returns_final_message_over_nats() {
    const CHILD_PROMPT: &str = "complete the delegated child work";
    const CHILD_FINAL: &str = "child final message over nats";
    const PARENT_FINAL: &str = "parent received child result";

    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let _nats_url = TestEnvGuard::new("HARNX_NATS_URL", &url);
    let _nats_token = TestEnvGuard::new("HARNX_NATS_TOKEN", "test-token");
    let agents_dir = seeded.config_dir().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("create agents directory");
    std::fs::write(
        agents_dir.join("metis.md"),
        "---\nmodel: test:test-model\n---\nConfigured metis agent\n",
    )
    .expect("write metis agent");
    let parent_session_id = crate::nats_worker::new_remote_session_id();
    let call_fn: crate::agent_loop::AgentCallFn = Arc::new(move |input, _config, _abort| {
        let has_tool_result = input.tool_calls.is_some();
        let prompt = input.text();
        Box::pin(async move {
            if has_tool_result {
                return Ok((
                    PARENT_FINAL.to_string(),
                    None,
                    vec![],
                    crate::client::CompletionTokenUsage::default(),
                ));
            }
            if prompt == CHILD_PROMPT {
                return Ok((
                    CHILD_FINAL.to_string(),
                    None,
                    vec![],
                    crate::client::CompletionTokenUsage::default(),
                ));
            }
            Ok((
                "delegating to child".to_string(),
                None,
                vec![ToolCall::new(
                    "metis_session_prompt".to_string(),
                    json!({ "message": CHILD_PROMPT }),
                    Some("nested-subagent-call".to_string()),
                    None,
                )],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    });
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);

    let client = async_nats::connect(&url)
        .await
        .expect("connect event observer to test nats");
    let mut child_events = client
        .subscribe("sessions.*.events")
        .await
        .expect("subscribe to session events");
    client.flush().await.expect("flush event subscription");
    let session = NatsSession::new(
        cluster_shared_session_config("local", parent_session_id.clone()),
        client.clone(),
        async_nats::jetstream::new(client.clone()),
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .expect("create parent NATS session");
    let parent_result = tokio::time::timeout(
        Duration::from_secs(15),
        session.run_turn(
            "delegate this request through the nested tool",
            Arc::new(NoopEventSink),
            None,
        ),
    )
    .await
    .expect("nested parent turn timed out")
    .expect("nested parent turn failed");
    assert_eq!(parent_result.response.as_deref(), Some(PARENT_FINAL));

    let parent_log = NatsSessionLog::new(
        async_nats::jetstream::new(client.clone()),
        parent_session_id.clone(),
    );
    let parent_entries = parent_log
        .load_events_async()
        .await
        .expect("load parent session log");
    let tool_output = parent_entries.iter().find_map(|(_, entry)| match entry {
        SessionLogEntry::ToolResults { results, .. } => results
            .iter()
            .find(|result| result.name == "metis_session_prompt")
            .map(|result| result.output.clone()),
        _ => None,
    });
    assert_eq!(
        tool_output
            .as_ref()
            .and_then(|value| value["response"].as_str()),
        Some(CHILD_FINAL),
        "nested tool result must contain the sub-agent's exact final message"
    );

    let child_session_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = child_events
                .next()
                .await
                .expect("session event subscription closed");
            let subject = event.subject.as_str();
            let session_id = subject
                .strip_prefix("sessions.")
                .and_then(|value| value.strip_suffix(".events"))
                .expect("session event subject shape");
            if session_id != parent_session_id {
                break session_id.to_string();
            }
        }
    })
    .await
    .expect("no child session event was visible on sessions.{child_id}.events");
    let child_stream_name = crate::nats_session_log::stream_name_for_session(&child_session_id);
    let jetstream = async_nats::jetstream::new(client);
    jetstream
        .get_stream(&child_stream_name)
        .await
        .expect("child SESSION_<id> JetStream log must exist");
    let child_entries = NatsSessionLog::new(jetstream, child_session_id)
        .load_events_async()
        .await
        .expect("load child session log");
    assert!(child_entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Message { role, content, .. }
            if role.is_assistant() && content.to_text() == CHILD_FINAL
    )));

    daemon.abort();
    let _ = daemon.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_agent_tool_family_and_nats_call_and_return_round_trip() {
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let _env_guard = env_lock().await;
    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    assert_remote_tool_family(&mut seeded.parent_config);
    assert_remote_tool_descriptions(&mut seeded.parent_config);

    let daemon = spawn_metis_worker(&url);
    let test_result = run_remote_round_trip(seeded.parent_config).await;

    daemon.abort();
    let _ = daemon.await;
    let _ = child.kill();
    let _ = child.wait();
    test_result.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_session_activation_writes_canonical_metadata_and_activity() {
    let _env_guard = env_lock().await;
    // Use an ISOLATED, self-provisioned NATS server (not the shared
    // HARNX_NATS_TEST_URL) so `run_turn` isn't slowed by
    // accumulated JetStream state on a shared server — that staleness made this
    // test hit its 10s turn timeout intermittently.
    let Some((server_url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&server_url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    assert_remote_tool_family(&mut seeded.parent_config);
    assert_remote_tool_descriptions(&mut seeded.parent_config);

    // Capture the session_id that will be created so we can assert on it specifically
    let expected_session_id = crate::nats_worker::new_remote_session_id();

    let client = async_nats::connect(&server_url)
        .await
        .expect("connect to test nats");
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .expect("ensure session metadata bucket");

    let daemon = spawn_metis_worker(&server_url);
    // Use the expected_session_id in the NATS session config
    let round_trip =
        run_remote_round_trip_with_session_id(seeded.parent_config, expected_session_id.clone())
            .await;

    // Assert on the specific session_id we created, not just "first key in bucket"
    let record = store
        .get(&expected_session_id)
        .await
        .expect("load session metadata")
        .expect("remote session metadata exists");
    assert_eq!(record.metadata.session_id, expected_session_id);
    assert_eq!(
        record.metadata.agent,
        SessionAgentSource::Named {
            name: "metis".to_string()
        }
    );
    let activity = store
        .get_activity(&record.metadata.session_id)
        .await
        .expect("load session activity")
        .expect("remote session activity exists");
    assert!(activity.first_activation_at.is_some());

    daemon.abort();
    let _ = daemon.await;
    round_trip.expect("remote round trip must succeed");
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_session_renew_updates_activity_without_clobbering_metadata() {
    let _env_guard = env_lock().await;
    // Isolated NATS (see activation test) so `run_turn` isn't delayed by shared
    // JetStream state into a 10s timeout.
    let Some((server_url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&server_url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    assert_remote_tool_family(&mut seeded.parent_config);
    assert_remote_tool_descriptions(&mut seeded.parent_config);

    // Capture the session_id that will be created so we can assert on it specifically
    let expected_session_id = crate::nats_worker::new_remote_session_id();

    let client = async_nats::connect(&server_url)
        .await
        .expect("connect to test nats");
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .expect("ensure session metadata bucket");

    // Arm the KV watcher BEFORE the session runs. `Store::watch` uses
    // DeliverPolicy::New (future updates only), so it must be established before
    // the initial index write and the lease-renewal refresh — otherwise both
    // Puts happen before the watcher exists and it waits forever.
    let record_key = activity_key(&expected_session_id);
    let mut watcher = store
        .kv_store()
        .watch(record_key.clone())
        .await
        .expect("watch session activity");

    // Fast lease renewal + a turn slow enough to stay active past a renew
    // interval, so a renewal (and its index `last_activity` refresh) fires while
    // the lease is still held — the worker releases the lease (aborting the
    // renew task) as soon as the turn goes idle.
    let daemon = spawn_metis_worker_with_fast_renew(
        &server_url,
        slow_prompt_call_fn("stub remote reply over nats", Duration::from_millis(1200)),
    );
    // Drive the turn concurrently; we observe the index writes via the watcher
    // while the session is active.
    let round_trip = tokio::spawn(run_remote_round_trip_with_session_id(
        seeded.parent_config,
        expected_session_id.clone(),
    ));

    // Collect Puts for our session in order: the first is the activation write,
    // a later one (strictly greater last_activity) is the renewal refresh.
    let mut first_record: Option<SessionActivity> = None;
    let mut refreshed_record: Option<SessionActivity> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && refreshed_record.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(entry)) = tokio::time::timeout(remaining, watcher.next()).await else {
            break;
        };
        let entry = entry.expect("watch entry result");
        if !matches!(entry.operation, async_nats::jetstream::kv::Operation::Put) {
            continue;
        }
        let record: SessionActivity =
            serde_json::from_slice(&entry.value).expect("deserialize watched session activity");
        match &first_record {
            None => first_record = Some(record),
            Some(first) if record.last_activity_at > first.last_activity_at => {
                refreshed_record = Some(record);
            }
            _ => {}
        }
    }

    let first_record = first_record.expect("activation should write initial session activity");
    let refreshed_record = refreshed_record.expect("renew should refresh last activity");
    assert!(refreshed_record.last_activity_at > first_record.last_activity_at);
    assert_eq!(
        refreshed_record.first_activation_at,
        first_record.first_activation_at
    );

    let metadata = store
        .get(&expected_session_id)
        .await
        .expect("load metadata")
        .expect("metadata exists");
    assert_eq!(
        metadata.metadata.agent,
        SessionAgentSource::Named {
            name: "metis".to_string()
        }
    );

    round_trip
        .await
        .expect("round trip task join")
        .expect("remote round trip must succeed");
    daemon.abort();
    let _ = daemon.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_dispatch_retract_round_trip() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = format!("dispatch-retract-{}", uuid::Uuid::new_v4());
    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let jetstream = seeded
        .parent_config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    seed_remote_dispatch_session_log(&log, &jetstream, &session_id)
        .await
        .expect("seed remote session log");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    delete_remote_message_range(&global_config, 2, 2, &abort)
        .await
        .expect("delete final user row");

    let entries = log
        .load_events_async()
        .await
        .expect("reload session log after retract");
    let edit = entries.last().expect("edit entry appended");
    match &edit.1 {
        SessionLogEntry::EditEntries {
            from,
            to,
            replacements,
        } => {
            assert_eq!(
                (*from, *to),
                (3, 3),
                "display index 2 must target JetStream seq 3"
            );
            assert!(
                replacements.is_empty(),
                "retract should append deletion edit"
            );
        }
        other => panic!("expected EditEntries, got {other:?}"),
    }
    assert_eq!(
        remote_dispatch_user_texts(&entries),
        vec!["first prompt".to_string()],
        "reconstructed state should only keep first user prompt after retract"
    );

    let _ = child.kill();
    let _ = child.wait();
}

// NATS JetStream mutation acknowledgements can hang indefinitely on Windows;
// the same platform-independent mutation behavior is covered on Unix CI.
#[cfg_attr(
    windows,
    ignore = "JetStream mutation acknowledgement hangs on Windows"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_dispatch_edit_round_trip() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = format!("dispatch-edit-{}", uuid::Uuid::new_v4());
    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let jetstream = seeded
        .parent_config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    seed_remote_dispatch_session_log(&log, &jetstream, &session_id)
        .await
        .expect("seed remote session log");
    let editor_tmp = tempfile::TempDir::new().expect("create editor temp dir");
    let editor_tmp_path = editor_tmp.path().to_path_buf();
    seeded.parent_config.temp_dir_override = Some(editor_tmp_path.clone());
    seeded.parent_config.set_tui_editor_hooks(
        None,
        Some(Box::new(move || {
            let temp_path = std::fs::read_dir(&editor_tmp_path)
                .expect("read editor temp dir")
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("txt"))
                .expect("message edit temp file");
            std::fs::write(&temp_path, "edited over dispatch").expect("write edited text");
        })),
    );

    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    edit_remote_message_range(&global_config, 2, 2, &abort)
        .await
        .expect("edit display index 2");

    let entries = log
        .load_events_async()
        .await
        .expect("reload session log after edit");
    let edit = entries.last().expect("edit entry appended");
    match &edit.1 {
        SessionLogEntry::EditEntries {
            from,
            to,
            replacements,
        } => {
            assert_eq!(
                (*from, *to),
                (3, 3),
                "display index 2 must target JetStream seq 3"
            );
            assert_eq!(replacements.len(), 1, "edit should append one replacement");
            let replacement = serde_yaml::from_str::<SessionLogEntry>(&replacements[0])
                .expect("replacement parses as session log entry");
            match replacement {
                SessionLogEntry::Message { role, content, .. } => {
                    assert!(role.is_user(), "replacement must stay user role");
                    assert_eq!(content.to_text(), "edited over dispatch");
                }
                other => panic!("expected replacement Message, got {other:?}"),
            }
        }
        other => panic!("expected EditEntries, got {other:?}"),
    }
    assert_eq!(
        remote_dispatch_user_texts(&entries),
        vec![
            "first prompt".to_string(),
            "edited over dispatch".to_string(),
        ],
        "reconstructed state should show edited user prompt after dispatch edit"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_delete_turn_matches_local_any_role_parity() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("worker should build realistic migrated session");

    let jetstream = seeded
        .parent_config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
    let log = NatsSessionLog::new(jetstream, session_id.clone());

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    delete_remote_message_range(&global_config, 1, 1, &abort)
        .await
        .expect("delete assistant row by logical row");

    let after_raw = log
        .load_events_async()
        .await
        .expect("reload session log after remote delete");
    let delete_entry = after_raw.last().expect("delete entry appended");
    match &delete_entry.1 {
        SessionLogEntry::EditEntries {
            from,
            to,
            replacements,
        } => {
            assert_eq!(
                (*from, *to),
                (2, 2),
                "assistant message must map to physical seq 2"
            );
            assert!(
                replacements.is_empty(),
                "delete must append empty replacements"
            );
        }
        other => panic!("expected EditEntries, got {other:?}"),
    }

    let after_effective =
        harnx_core::session_reconstruct::reconstruct_state_from_nats(&after_raw).next_turn_messages;
    assert!(
        !after_effective
            .iter()
            .any(|message| message.role.is_assistant()),
        "assistant turn should be gone after delete"
    );
    assert_eq!(
        after_effective
            .iter()
            .filter(|message| message.role.is_user())
            .count(),
        1,
        "initial user prompt should remain"
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_delete_accepts_first_transcript_row() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let client = async_nats::connect(&url)
        .await
        .expect("connect nats client");
    let jetstream = async_nats::jetstream::new(client);
    SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .expect("ensure metadata store")
        .create(&SessionMetadata::new(
            &session_id,
            crate::SessionInitializer::named("metis", Default::default()),
        ))
        .await
        .expect("create canonical metadata");
    let log = NatsSessionLog::new(jetstream, session_id.clone());
    let first_user_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some(uuid::Uuid::new_v4().to_string()),
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("first prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await
        .expect("append first user message");
    assert_eq!(first_user_seq, 1, "first physical row is the user message");

    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("worker should load canonical session");

    let mut parent_config = seeded.parent_config;
    parent_config.set_remote_agent("metis".to_string(), "local".to_string());
    parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    delete_remote_message_range(&global_config, 0, 0, &abort)
        .await
        .expect("the first conversation row is editable");
    assert!(matches!(
        log.load_events_async()
            .await
            .expect("reload transcript")
            .last(),
        Some((_, SessionLogEntry::EditEntries { from: 1, to: 1, replacements }))
            if replacements.is_empty()
    ));

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_rewind_appends_mutation_without_truncating_stream() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("worker should build realistic migrated session");

    let jetstream = seeded
        .parent_config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
    let log = NatsSessionLog::new(jetstream, session_id.clone());
    let before_raw = log
        .load_events_async()
        .await
        .expect("load raw session entries before rewind");
    let raw_len_before = before_raw.len();

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    rewind_remote_session(&global_config, 0, &abort)
        .await
        .expect("rewind to first conversation row");

    let after_raw = log
        .load_events_async()
        .await
        .expect("load raw session entries after rewind");
    assert!(
        after_raw.len() > raw_len_before,
        "rewind must append mutation(s) instead of truncating stream"
    );
    // The new implementation uses group-aware EditEntries instead of Rewind
    // for logical suffix deletion (correct for shared-seq groups)
    let last = after_raw.last().expect("mutation entry appended");
    match &last.1 {
        SessionLogEntry::Rewind { after_seq } => {
            assert_eq!(*after_seq, 1, "logical row 0 must map to physical seq 1");
        }
        SessionLogEntry::EditEntries {
            from,
            to,
            replacements,
        } => {
            assert!(*from > 0, "edit entries must target transcript sequences");
            assert_eq!(*from, *to, "exact-set deletion uses from==to");
            assert!(
                replacements.is_empty(),
                "suffix deletion has empty replacements"
            );
        }
        other => panic!("expected Rewind or EditEntries, got {other:?}"),
    }

    let effective_after = load_effective_entries(&log).await;
    let effective_texts: Vec<String> = effective_after
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        effective_texts,
        vec!["delegate over nats".to_string()],
        "rewind should leave only header + chosen user prompt in effective history"
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_delete_refreshes_after_concurrent_mutation() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("worker should build realistic migrated session");

    let jetstream = seeded
        .parent_config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
    let log = NatsSessionLog::new(jetstream, session_id.clone());

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    let stale_state = load_remote_session_for_render(
        &NatsSession::from_global_config(
            cluster_shared_session_config("local", session_id.clone()),
            &global_config,
            abort.clone(),
        )
        .await
        .expect("load session"),
    )
    .await
    .expect("capture stale state");

    log.append_event_async(&SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("late user".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await
    .expect("append concurrent late user message");

    let stale_attempt = log
        .append_event_with_expected_last_sequence_async(
            &SessionLogEntry::EditEntries {
                from: 2,
                to: 2,
                replacements: vec![],
            },
            stale_state.last_seen_stream_seq,
        )
        .await;
    assert!(stale_attempt.is_err(), "stale cas append must fail");

    delete_remote_message_range(&global_config, 1, 1, &abort)
        .await
        .expect("refreshing delete should retry and succeed");

    let after_raw = log
        .load_events_async()
        .await
        .expect("load raw session entries after concurrent delete");
    let late_user_still_present =
        harnx_core::session_reconstruct::reconstruct_state_from_nats(&after_raw)
            .next_turn_messages
            .iter()
            .any(|message| message.content.to_text() == "late user");
    assert!(
        late_user_still_present,
        "retry delete must not target concurrent late row"
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg_attr(
    windows,
    ignore = "JetStream mutation acknowledgement hangs on Windows"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_edit_preserves_canonical_transcript_messages() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("first turn");
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("second turn");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    edit_remote_message_range(&global_config, 0, 0, &abort)
        .await
        .expect("edit older user message");

    let session = NatsSession::from_global_config(
        cluster_shared_session_config("local", session_id.clone()),
        &global_config,
        abort,
    )
    .await
    .expect("load session");
    let state = load_remote_session_for_render(&session)
        .await
        .expect("load remote render state");

    // Verify all expected messages are present
    let texts: Vec<String> = state
        .logical_entries
        .iter()
        .filter_map(|entry| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts.len(),
        4,
        "after edit should have 4 messages (U1, A1, U2, A2)"
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg_attr(
    windows,
    ignore = "JetStream mutation acknowledgement hangs on Windows"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_delete_after_older_edit_deletes_exact_late_range() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("first turn");
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("second turn");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    edit_remote_message_range(&global_config, 0, 0, &abort)
        .await
        .expect("edit older user message");
    delete_remote_message_range(&global_config, 2, 3, &abort)
        .await
        .expect("delete later logical range");

    let session = NatsSession::from_global_config(
        cluster_shared_session_config("local", session_id.clone()),
        &global_config,
        abort,
    )
    .await
    .expect("load session");
    let state = load_remote_session_for_render(&session)
        .await
        .expect("load remote render state");
    let texts: Vec<String> = state
        .logical_entries
        .iter()
        .filter_map(|entry| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "delegate over nats".to_string(),
            "stub remote reply over nats".to_string(),
        ]
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg_attr(
    windows,
    ignore = "JetStream mutation acknowledgement hangs on Windows"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_rewind_after_older_edit_preserves_correct_logical_prefix() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("first turn");
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("second turn");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    edit_remote_message_range(&global_config, 0, 0, &abort)
        .await
        .expect("edit older user message");
    rewind_remote_session(&global_config, 1, &abort)
        .await
        .expect("rewind logical suffix");

    let session = NatsSession::from_global_config(
        cluster_shared_session_config("local", session_id.clone()),
        &global_config,
        abort,
    )
    .await
    .expect("load session");
    let state = load_remote_session_for_render(&session)
        .await
        .expect("load remote render state");
    let texts: Vec<String> = state
        .logical_entries
        .iter()
        .filter_map(|entry| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "delegate over nats".to_string(),
            "stub remote reply over nats".to_string(),
        ]
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg_attr(
    windows,
    ignore = "JetStream mutation acknowledgement hangs on Windows"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_delete_command_routes_to_exact_set_mutations() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("first turn");
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("second turn");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    edit_remote_message_range(&global_config, 0, 0, &abort)
        .await
        .expect("edit older user message");
    crate::commands::run_command(&global_config, abort.clone(), ".delete message 2-3")
        .await
        .expect("remote delete command succeeds");

    let session = NatsSession::from_global_config(
        cluster_shared_session_config("local", session_id.clone()),
        &global_config,
        abort.clone(),
    )
    .await
    .expect("load session");
    let state = load_remote_session_for_render(&session)
        .await
        .expect("load remote render state");
    let texts: Vec<String> = state
        .logical_documents
        .iter()
        .map(|doc| serde_yaml::from_str::<SessionLogEntry>(doc).expect("deserialize logical doc"))
        .filter_map(|entry| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "delegate over nats".to_string(),
            "stub remote reply over nats".to_string(),
        ]
    );

    // Fetch jetstream without holding lock across await
    let js = {
        let cfg = global_config.read();
        cfg.nats_server("local").expect("nats server").clone()
    };
    let js = Config::connect_nats_server(&js)
        .await
        .expect("connect to jetstream");
    let js = async_nats::jetstream::new(js);
    let raw = NatsSessionLog::new(js, session_id)
        .load_events_async()
        .await
        .expect("raw log");
    // Check reconstructed state instead of counting mutation shapes
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)
        .expect("reconstruct effective log");
    let effective_texts: Vec<String> = effective
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        effective_texts,
        vec![
            "delegate over nats".to_string(),
            "stub remote reply over nats".to_string(),
        ],
        "reconstructed log must have correct message content"
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg_attr(
    windows,
    ignore = "JetStream mutation acknowledgement hangs on Windows"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_rewind_command_routes_to_exact_suffix_deletions() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("first turn");
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("second turn");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    edit_remote_message_range(&global_config, 0, 0, &abort)
        .await
        .expect("edit older user message");
    crate::commands::run_command(&global_config, abort.clone(), ".rewind 1")
        .await
        .expect("remote rewind command succeeds");

    let session = NatsSession::from_global_config(
        cluster_shared_session_config("local", session_id.clone()),
        &global_config,
        abort.clone(),
    )
    .await
    .expect("load session");
    let state = load_remote_session_for_render(&session)
        .await
        .expect("load remote render state");
    let texts: Vec<String> = state
        .logical_documents
        .iter()
        .map(|doc| serde_yaml::from_str::<SessionLogEntry>(doc).expect("deserialize logical doc"))
        .filter_map(|entry| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "delegate over nats".to_string(),
            "stub remote reply over nats".to_string(),
        ]
    );

    // Fetch jetstream without holding lock across await
    let js = {
        let cfg = global_config.read();
        cfg.nats_server("local").expect("nats server").clone()
    };
    let js = Config::connect_nats_server(&js)
        .await
        .expect("connect to jetstream");
    let js = async_nats::jetstream::new(js);
    let raw = NatsSessionLog::new(js, session_id)
        .load_events_async()
        .await
        .expect("raw log");
    let rewind_entries = raw
        .iter()
        .filter(|(_, entry)| matches!(entry, SessionLogEntry::Rewind { .. }))
        .count();
    assert_eq!(
        rewind_entries, 0,
        "remote rewind should not emit physical-cutoff Rewind entries"
    );
    // Check reconstructed state instead of counting mutation shapes
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)
        .expect("reconstruct effective log");
    let effective_texts: Vec<String> = effective
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        effective_texts,
        vec![
            "delegate over nats".to_string(),
            "stub remote reply over nats".to_string(),
        ],
        "reconstructed log must have correct message content"
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

/// Multiple leading user messages remain distinct logical rows even when they
/// precede a worker activation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_remote_transcript_multi_leading_user_rows_are_distinct() {
    use harnx_core::message::{MessageContent, MessageRole};
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    let session_id = crate::nats_worker::new_remote_session_id();

    // Seed two leading user messages directly to the durable log BEFORE the
    // worker activates, mirroring a client that appended several prompts
    // before the first worker turn.
    let jetstream = seeded
        .parent_config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
    SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .expect("ensure canonical session metadata store")
        .create(&SessionMetadata::new(
            &session_id,
            crate::SessionInitializer::named("metis", Default::default()),
        ))
        .await
        .expect("seed canonical session metadata");
    let seed_log = NatsSessionLog::new(jetstream, session_id.clone());
    for text in ["leading one", "leading two"] {
        seed_log
            .append_event_async(&SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text(text.to_string()),
                timestamp: None,
                fence_token: None,
            })
            .await
            .expect("seed leading user message");
    }

    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("worker loads multi-leading-user session");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();
    let session = NatsSession::from_global_config(
        cluster_shared_session_config("local", session_id.clone()),
        &global_config,
        abort,
    )
    .await
    .expect("load session");

    let transcript = load_remote_transcript_for_render(&session)
        .await
        .expect("load transcript state");

    let row_seqs: Vec<usize> = transcript
        .messages
        .iter()
        .filter_map(|message| message.log_seq)
        .collect();
    // Metadata is outside the transcript. Rows are numbered from zero:
    // leading one=0, leading two=1, "delegate over nats"=2, assistant=3.
    assert_eq!(
        row_seqs,
        vec![0, 1, 2, 3],
        "shared-seq leading-user rows must get distinct contiguous logical seqs, not collapse"
    );
    let mut sorted = row_seqs.clone();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        row_seqs.len(),
        "resumed row seqs must be unique"
    );

    let row_texts: Vec<(MessageRole, String)> = transcript
        .messages
        .iter()
        .map(|message| (message.role, message.content.to_text()))
        .collect();
    assert_eq!(
        row_texts,
        vec![
            (MessageRole::User, "leading one".to_string()),
            (MessageRole::User, "leading two".to_string()),
            (MessageRole::User, "delegate over nats".to_string()),
            (
                MessageRole::Assistant,
                "stub remote reply over nats".to_string()
            ),
        ]
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}
