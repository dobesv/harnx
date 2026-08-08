//! Unit tests for nats_worker module.

use crate::config::remote_session_ops::{
    delete_remote_message_range, edit_remote_message_range, load_remote_session_for_render,
    load_remote_transcript_for_render, rewind_remote_session,
};
use crate::config::{self, Config};
use crate::nats_session_index::{
    ensure_index_bucket, get_record, session_index_key, SessionIndexRecord,
};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_worker::agent_loop::{tool_can_rerun, write_header_and_load_session};
use crate::nats_worker::run_worker_daemon;
use crate::ThinClientSession;
use anyhow::{bail, Context};
use futures_util::StreamExt;
use harnx_core::agent_config::AgentConfig;
use harnx_core::config_data::ConfigData;
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::session_reconstruct::{
    active_context_window, reconstruct_state_from_nats, TurnStatus,
};
use harnx_core::tool::{ToolCall, ToolDeclaration, ToolProvider};
use harnx_toolset::Toolset;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

/// Spawn a local JetStream-enabled nats-server on a free port with an isolated
/// temp store dir, returning the connect URL, the child process, and the temp
/// dir guard. Using a free port + per-run store dir avoids cross-run state
/// bleed (JetStream KV/lease buckets) and port collisions that make tests flaky
/// when run repeatedly or in parallel. Returns `None` if nats-server is absent.
pub(super) async fn spawn_test_nats() -> Option<(String, std::process::Child, tempfile::TempDir)> {
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

static CWD_MUTEX: LazyLock<std::sync::Mutex<()>> = LazyLock::new(|| std::sync::Mutex::new(()));
static ENV_MUTEX: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

struct CurrentDirGuard {
    original_dir: PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl CurrentDirGuard {
    fn change_to(path: &Path) -> std::io::Result<Self> {
        let lock = CWD_MUTEX.lock().unwrap();
        let original_dir = std::env::current_dir()?;
        std::env::set_current_dir(path)?;
        Ok(Self {
            original_dir,
            _lock: lock,
        })
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.original_dir)
            .expect("must restore original working directory after test");
    }
}

pub(super) async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_MUTEX.lock().await
}

pub(super) struct TestEnvGuard {
    key: String,
    prev: Option<std::ffi::OsString>,
}

impl TestEnvGuard {
    pub(super) fn new(key: &str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            prev,
        }
    }
}

impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => unsafe { std::env::set_var(&self.key, value) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

fn load_config_via_internal_pipeline(config_path: &Path) -> Config {
    let prev = std::env::var_os("HARNX_CONFIG_DIR");
    let config_dir = config_path
        .parent()
        .expect("config path must have parent directory");
    let _config_guard = TestEnvGuard::new("HARNX_CONFIG_DIR", config_dir);
    let config = Config::load_from_file(config_path).unwrap();
    drop(_config_guard);
    match prev {
        Some(value) => unsafe { std::env::set_var("HARNX_CONFIG_DIR", value) },
        None => unsafe { std::env::remove_var("HARNX_CONFIG_DIR") },
    }
    config
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
    session_id: &str,
) -> anyhow::Result<()> {
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::Session;

    let header_session = Session {
        id: session_id.to_string(),
        model_id: "test:test-model".to_string(),
        agent_name: Some("metis".to_string()),
        session_id: Some(session_id.to_string()),
        ..Default::default()
    };
    log.append_event_async(&header_session.build_header_entry())
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
    let mut daemon = crate::nats_worker::WorkerDaemonConfig::new("local", "worker-metis");
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
        crate::nats_worker::WorkerDaemonConfig::new("local", "worker-metis"),
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

pub(super) async fn run_remote_round_trip_with_session_id_and_sink(
    parent_config: Config,
    session_id: String,
    sink: Arc<dyn AgentEventSink>,
    cluster: &str,
) -> anyhow::Result<()> {
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let mut parent_config = parent_config;
    let parent_session =
        crate::config::session::new(&parent_config, "parent-nats-roundtrip", None)?;
    parent_config.session = Some(parent_session);
    let parent_global_config = Arc::new(parking_lot::RwLock::new(parent_config));
    let abort_signal = harnx_core::abort::create_abort_signal();
    let thin_cfg = crate::ThinClientConfig {
        cluster: cluster.to_string(),
        agent: "metis".to_string(),
        session_id: Some(session_id),
    };
    let thin =
        crate::ThinClientSession::from_global_config(thin_cfg, &parent_global_config, abort_signal)
            .await?;

    let turn_result = tokio::time::timeout(
        Duration::from_secs(10),
        thin.run_turn("delegate over nats", sink, None),
    )
    .await
    .context("thin client run_turn timed out after 10s in remote NATS round-trip test")??;
    let reply = turn_result
        .response
        .context("thin client turn must return final assistant response")?;
    anyhow::ensure!(
        reply.contains("stub remote reply over nats"),
        "expected reply to contain stub remote reply, got: {reply}"
    );
    Ok(())
}
fn run_git(temp_repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(temp_repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git command must spawn");
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Drop the `session_id:` line from a serialized header so two independently
/// built headers can be compared ignoring their random session ids.
fn strip_session_id_line(yaml: &str) -> String {
    yaml.lines()
        .filter(|line| !line.trim_start().starts_with("session_id:"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn create_test_git_repo() -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("temp dir must be created");
    let repo = temp_dir.path();
    let tracked_file = repo.join("README.md");

    run_git(repo, &["init", "-b", "main"]);
    std::fs::write(&tracked_file, "hermetic test repo\n").expect("tracked file must be written");
    run_git(repo, &["add", "README.md"]);
    run_git(
        repo,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-m",
            "initial commit",
            "--no-gpg-sign",
            "--no-verify",
        ],
    );
    run_git(repo, &["checkout", "-b", "test-branch"]);
    run_git(
        repo,
        &[
            "remote",
            "add",
            "origin",
            "https://example.com/test/repo.git",
        ],
    );

    temp_dir
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
async fn remote_header_matches_local_header_source_of_truth() {
    // Changes the process-global current directory (via CurrentDirGuard) to
    // generate the header from a hermetic git repo. Under `cargo test` that cwd
    // mutation races every other test that resolves paths against the cwd;
    // nextest's per-test process isolation is required to keep it hermetic.
    harnx_core::require_nextest();
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let temp_repo = create_test_git_repo();
    // Canonicalize so the expected `working_dir` matches the physical path that
    // `std::env::current_dir()` returns after `set_current_dir`. On platforms
    // where the temp root is a symlink (macOS `/var` -> `/private/var`, or a
    // symlinked `TMPDIR`), the raw temp path and the resolved cwd differ.
    let temp_repo_path = temp_repo
        .path()
        .canonicalize()
        .expect("temp repo path must canonicalize");
    let original_cwd = std::env::current_dir().expect("current dir must exist before test");
    let client = async_nats::connect(&url).await.unwrap();
    let jetstream = async_nats::jetstream::new(client);
    let session_id = "remote-header-test";
    let backend = crate::nats_worker::backend::NatsSessionLogBackend::new(jetstream, session_id);

    let mut config = config::Config {
        data: ConfigData {
            save_session: Some(false),
            compress_threshold: 17,
            ..Default::default()
        },
        ..Default::default()
    };
    config.set_model_fallbacks(vec!["openai:gpt-4o-mini".to_string()]);
    config.set_compaction_agent(Some("pkg/compactor".to_string()));
    let agent = config::Agent::new(
        AgentConfig::from_markdown(
            "pkg/main",
            "---\nmodel: openai:gpt-4.1\nsave_session: false\n---\nAgent instructions without variables.",
        )
        .unwrap(),
    );
    config.agent = Some(agent.clone());
    let config = Arc::new(parking_lot::RwLock::new(config));

    let input = harnx_core::input::Input::new(
        "hello".to_string(),
        ("hello".to_string(), vec![]),
        agent.into_config(),
    );

    let (session, expected_header) = {
        let _cwd_guard = CurrentDirGuard::change_to(&temp_repo_path)
            .expect("must switch into hermetic git repo for header generation");
        let session =
            write_header_and_load_session(&backend, &config, &input, None, session_id, None)
                .await
                .unwrap();
        let expected_header = {
            let mut expected_session =
                config::session::new(&config.read(), session_id, None).unwrap();
            expected_session.set_agent(&input.agent).unwrap();
            expected_session.build_header_entry()
        };
        (session, expected_header)
    };

    assert_eq!(
        std::env::current_dir().unwrap(),
        original_cwd,
        "test must restore original working directory"
    );

    let entries = backend.load_events_blocking().unwrap();
    let actual_header = entries
        .iter()
        .find(|(_, entry)| matches!(entry, harnx_core::session::SessionLogEntry::Header { .. }))
        .expect("header entry present");
    let actual_header_yaml = serde_yaml::to_string(&actual_header.1).unwrap();
    let expected_header_yaml = serde_yaml::to_string(&expected_header).unwrap();
    assert!(actual_header_yaml.contains("agent_name: pkg/main"));
    assert!(actual_header_yaml.contains("save_session: false"));
    assert!(
        actual_header_yaml.contains("agent_instructions: Agent instructions without variables.")
    );
    assert!(actual_header_yaml.contains("git_branch: test-branch"));
    assert!(
        actual_header_yaml.contains("git_remote: https://example.com/test/repo.git"),
        "actual header must use hermetic git remote: {actual_header_yaml}"
    );
    let expected_working_dir = format!("working_dir: {}", temp_repo_path.display());
    assert!(
        actual_header_yaml.contains(&expected_working_dir),
        "actual header must use hermetic working dir: {actual_header_yaml}"
    );
    // Each header is built from an INDEPENDENT `session::new` call, so each
    // generates its own random `session_id`. Normalize that legitimately
    // non-deterministic line out before comparing the rest (which must match
    // exactly) — otherwise the test is flaky under nextest parallelism. This
    // equality also covers the git_branch/git_remote/working_dir fields, so no
    // separate `expected_header_yaml.contains(...)` assertions are needed.
    assert_eq!(
        strip_session_id_line(&actual_header_yaml),
        strip_session_id_line(&expected_header_yaml),
        "remote header must match locally built header (ignoring random session_id)"
    );
    let loaded_header_yaml = serde_yaml::to_string(&session.build_header_entry()).unwrap();
    assert!(loaded_header_yaml.contains("agent_name: pkg/main"));
    assert!(expected_header_yaml.contains("agent_name: pkg/main"));
    assert!(loaded_header_yaml.contains("git_branch: test-branch"));

    let _ = child.kill();
    let _ = child.wait();
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
        session_index: None,
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

#[derive(Default)]
struct RecordingEventSink {
    events: std::sync::Mutex<Vec<AgentEvent>>,
}

impl RecordingEventSink {
    fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl AgentEventSink for RecordingEventSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
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
        "local",
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

async fn run_remote_turn_returning_reply(
    parent_config: Config,
    session_id: String,
    prompt: &str,
) -> anyhow::Result<String> {
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let mut parent_config = parent_config;
    let parent_session =
        crate::config::session::new(&parent_config, "parent-nats-roundtrip", None)?;
    parent_config.session = Some(parent_session);
    let parent_global_config = Arc::new(parking_lot::RwLock::new(parent_config));
    let abort_signal = harnx_core::abort::create_abort_signal();
    let thin_cfg = crate::ThinClientConfig {
        cluster: "local".to_string(),
        agent: "metis".to_string(),
        session_id: Some(session_id),
    };
    let thin =
        crate::ThinClientSession::from_global_config(thin_cfg, &parent_global_config, abort_signal)
            .await?;

    let turn_result = tokio::time::timeout(
        Duration::from_secs(10),
        thin.run_turn(prompt, Arc::new(NoopEventSink), None),
    )
    .await
    .context("thin client run_turn timed out after 10s in remote NATS round-trip test")??;
    turn_result
        .response
        .context("thin client turn must return final assistant response")
}

fn leading_user_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { role, content, .. } if role.is_user() => {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect()
}

async fn load_effective_entries(log: &NatsSessionLog) -> Vec<(u64, SessionLogEntry)> {
    let entries = log
        .load_events_async()
        .await
        .expect("load session log entries");
    harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)
        .expect("reconstruct effective session log")
}

async fn assert_remote_header_inserted_once(log: &NatsSessionLog, expected_first_prompt: &str) {
    let raw_entries = log
        .load_events_async()
        .await
        .expect("load raw session log entries");
    let effective_entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw_entries)
        .expect("reconstruct effective session log");

    assert!(
        matches!(
            effective_entries.first().map(|(_, entry)| entry),
            Some(SessionLogEntry::Header { .. })
        ),
        "effective log must start with header after worker activation"
    );
    assert_eq!(
        leading_user_texts(&effective_entries)
            .first()
            .map(std::string::String::as_str),
        Some(expected_first_prompt),
        "first user prompt must remain in active window after migration"
    );

    let edits: Vec<_> = raw_entries
        .iter()
        .filter(|(_, entry)| matches!(entry, SessionLogEntry::EditEntries { .. }))
        .collect();
    assert_eq!(
        edits.len(),
        1,
        "worker must append exactly one migration EditEntries"
    );

    match &edits[0].1 {
        SessionLogEntry::EditEntries {
            from,
            to,
            replacements,
        } => {
            assert!(
                replacements.len() >= 2,
                "migration must prepend header and clone users"
            );
            let header_replacement = serde_yaml::from_str::<SessionLogEntry>(&replacements[0])
                .expect("header replacement parses");
            assert!(matches!(header_replacement, SessionLogEntry::Header { .. }));
            assert_eq!(
                *from, 1,
                "headerless realistic origin starts at first user seq"
            );
            assert_eq!(
                *to, 1,
                "single-prompt fixture edits leading user block only"
            );
        }
        other => panic!("expected EditEntries migration, got {other:?}"),
    }
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
async fn remote_headerless_session_inserts_header_on_first_activation() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    assert_remote_tool_family(&mut seeded.parent_config);
    assert_remote_tool_descriptions(&mut seeded.parent_config);

    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));
    let session_id = crate::nats_worker::new_remote_session_id();
    run_remote_round_trip_with_session_id(seeded.parent_config, session_id.clone())
        .await
        .expect("run realistic headerless thin-client session");

    let log_client = async_nats::connect(&url)
        .await
        .expect("connect nats client for session log verification");
    let log = NatsSessionLog::new(async_nats::jetstream::new(log_client), session_id);
    assert_remote_header_inserted_once(&log, "delegate over nats").await;

    worker.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), worker).await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_multi_turn_after_header_insert_preserves_input_derivation() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    assert_remote_tool_family(&mut seeded.parent_config);
    assert_remote_tool_descriptions(&mut seeded.parent_config);

    let prompts = ["turn one prompt", "turn two prompt", "turn three prompt"];
    let captured = Arc::new(AsyncMutex::new(Vec::<String>::new()));
    let worker = spawn_metis_worker_with_call_fn(&url, echoing_call_fn(Arc::clone(&captured)));
    let session_id = crate::nats_worker::new_remote_session_id();

    for prompt in prompts {
        let reply = run_remote_turn_returning_reply(
            seeded.parent_config.clone(),
            session_id.clone(),
            prompt,
        )
        .await
        .expect("run remote thin-client turn");
        assert_eq!(
            reply,
            format!("stub remote reply over nats: {prompt}"),
            "assistant reply must echo exact derived turn input"
        );
    }

    worker.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), worker).await;

    let derived_inputs = captured.lock().await.clone();
    assert_eq!(
        derived_inputs,
        prompts
            .iter()
            .map(|prompt| (*prompt).to_string())
            .collect::<Vec<_>>(),
        "worker must derive exactly one turn-input per turn, in order, with no dup/drop"
    );

    let log_client = async_nats::connect(&url)
        .await
        .expect("connect nats client for session log verification");
    let log = NatsSessionLog::new(async_nats::jetstream::new(log_client), session_id);
    let raw_entries = log
        .load_events_async()
        .await
        .expect("load raw session log entries");
    let effective_entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw_entries)
        .expect("reconstruct effective session log");

    let edit_count = raw_entries
        .iter()
        .filter(|(_, entry)| matches!(entry, SessionLogEntry::EditEntries { .. }))
        .count();
    assert_eq!(
        edit_count, 1,
        "exactly one header-migration EditEntries across whole multi-turn session"
    );

    let header_count = effective_entries
        .iter()
        .filter(|(_, entry)| matches!(entry, SessionLogEntry::Header { .. }))
        .count();
    assert_eq!(
        header_count, 1,
        "effective log must carry exactly one header"
    );
    let Some((_, SessionLogEntry::Header { agent_name, .. })) = effective_entries.first() else {
        panic!("effective log must start with migrated header at logical 0");
    };
    assert_eq!(
        agent_name.as_deref(),
        Some("metis"),
        "migrated header must carry worker agent config"
    );

    let user_texts = leading_user_texts(&effective_entries);
    assert_eq!(
        user_texts,
        prompts
            .iter()
            .map(|prompt| (*prompt).to_string())
            .collect::<Vec<_>>(),
        "effective log user messages must be exact prompt sequence, once each"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_headerless_session_reactivation_is_idempotent() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    assert_remote_tool_family(&mut seeded.parent_config);

    let first_worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));
    let session_id = crate::nats_worker::new_remote_session_id();
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("first activation round trip succeeds");
    first_worker.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), first_worker).await;

    let second_worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));
    run_remote_round_trip_with_session_id(seeded.parent_config, session_id.clone())
        .await
        .expect("reactivation round trip succeeds");

    let log_client = async_nats::connect(&url)
        .await
        .expect("connect nats client for session log verification");
    let log = NatsSessionLog::new(async_nats::jetstream::new(log_client), session_id);
    let raw_entries = log
        .load_events_async()
        .await
        .expect("load raw session log entries");
    let edit_count = raw_entries
        .iter()
        .filter(|(_, entry)| matches!(entry, SessionLogEntry::EditEntries { .. }))
        .count();
    assert_eq!(
        edit_count, 1,
        "reactivation must not append a second header migration edit"
    );
    let effective_entries = load_effective_entries(&log).await;
    assert!(matches!(
        effective_entries.first().map(|(_, entry)| entry),
        Some(SessionLogEntry::Header { .. })
    ));

    second_worker.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), second_worker).await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_headerless_session_migrates_on_first_activation() {
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
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    let user_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some(uuid::Uuid::new_v4().to_string()),
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("legacy prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await
        .expect("append legacy user message");
    assert_eq!(
        user_seq, 1,
        "legacy fixture should be truly headerless at origin"
    );

    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));
    run_remote_round_trip_with_session_id(seeded.parent_config, session_id.clone())
        .await
        .expect("worker should migrate legacy session then answer turn");

    let raw_entries = log
        .load_events_async()
        .await
        .expect("load raw entries after migration");
    let effective_after = load_effective_entries(&log).await;
    assert!(matches!(
        effective_after.first().map(|(_, entry)| entry),
        Some(SessionLogEntry::Header { .. })
    ));
    assert!(
        raw_entries
            .iter()
            .all(|(_, entry)| !matches!(entry, SessionLogEntry::Header { .. })),
        "migration must not physically reorder or prepend raw header entries"
    );
    assert_eq!(
        leading_user_texts(&effective_after),
        vec![
            "legacy prompt".to_string(),
            "delegate over nats".to_string()
        ]
    );

    worker.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), worker).await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_replay_and_live_rows_emit_logical_seq_assignments() {
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
    let log = NatsSessionLog::new(jetstream, session_id.clone());
    let legacy_user_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some(uuid::Uuid::new_v4().to_string()),
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("legacy prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await
        .expect("append legacy user message");
    assert_eq!(
        legacy_user_seq, 1,
        "fixture must start headerless so worker performs realistic migration"
    );

    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));
    let sink = Arc::new(RecordingEventSink::default());
    run_remote_round_trip_with_session_id_and_sink(
        seeded.parent_config,
        session_id.clone(),
        sink.clone(),
        "local",
    )
    .await
    .expect("worker should migrate legacy session, replay history, then answer turn");

    let replayed_seqs: Vec<usize> = sink
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) => Some(seq),
            _ => None,
        })
        .collect();

    tokio::time::sleep(Duration::from_millis(250)).await;
    let raw_entries = log
        .load_events_async()
        .await
        .expect("load raw entries after replay/live turn");
    let effective_entries = load_effective_entries(&log).await;
    let logical_indices: Vec<usize> = active_context_window(&effective_entries)
        .logical_entries()
        .map(|entry| entry.logical_index)
        .collect();
    assert_eq!(
        logical_indices,
        vec![0, 1, 2, 3],
        "effective migrated session must expose contiguous logical numbering"
    );
    let physical_seqs: Vec<u64> = active_context_window(&effective_entries)
        .logical_entries()
        .map(|entry| entry.physical_seq)
        .collect();
    let user_message_count = raw_entries
        .iter()
        .filter(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, .. } if role.is_user()
            )
        })
        .count();
    let assistant_message_count = raw_entries
        .iter()
        .filter(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, .. } if role.is_assistant()
            )
        })
        .count();
    assert_eq!(
        user_message_count, 2,
        "realistic round trip must durably append legacy + live user messages"
    );
    assert_eq!(
        assistant_message_count, 1,
        "realistic round trip must durably append one assistant reply"
    );
    assert_eq!(
        physical_seqs,
        vec![3, 3, 3, 4],
        "migrated active window should coalesce migrated header/user/live user onto worker turn seq, then assistant reply"
    );
    // The migrated active window is logically [Header(0), legacy user(1), live
    // user(2), assistant(3)] — but the Header renders NO transcript row, so it
    // emits no LogSeqAssigned (exactly like a LOCAL session, whose header is
    // document 0 and whose first USER message is log_seq 1). The numberable
    // rows therefore carry logical seqs [1, 2, 3]: replayed migrated legacy
    // user, live thin-client user, then live worker assistant. This is the
    // local==remote numbering parity that makes resumed/live remote rows
    // targetable for edit/delete/rewind.
    assert_eq!(
        replayed_seqs,
        vec![1, 2, 3],
        "sink should number the migrated legacy user, live thin-client user, and live worker assistant rows (Header at logical 0 renders no row, matching local numbering)"
    );
    assert_eq!(
        replayed_seqs
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        replayed_seqs.len(),
        "each logical seq should emit once; duplicates would wedge TUI numbering"
    );

    worker.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), worker).await;
    let _ = child.kill();
    let _ = child.wait();
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
    let call_fn: crate::agent_loop::AgentCallFn = {
        let entered = Arc::clone(&entered);
        let worker_saw_abort = Arc::clone(&worker_saw_abort);
        Arc::new(move |_input, _config, abort| {
            let entered = Arc::clone(&entered);
            let worker_saw_abort = Arc::clone(&worker_saw_abort);
            Box::pin(async move {
                entered.notify_one();
                tokio::select! {
                    _ = harnx_core::abort::wait_abort_signal(&abort) => {
                        worker_saw_abort.store(true, Ordering::SeqCst);
                        loop {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
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
    let thin_cfg = crate::ThinClientConfig {
        cluster: "local".to_string(),
        agent: "metis".to_string(),
        session_id: Some(session_id.clone()),
    };
    let thin =
        crate::ThinClientSession::from_global_config(thin_cfg, &parent_global_config, abort_signal)
            .await
            .expect("build thin client session");

    let run_turn = tokio::spawn(async move {
        thin.run_turn("delegate over nats", Arc::new(NoopEventSink), None)
            .await
    });

    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("worker never entered in-flight call_fn before cancel publish");

    let raw_client = async_nats::connect(&url)
        .await
        .expect("connect raw nats client for cancel publish");
    crate::send_control_command(&raw_client, &session_id, crate::ControlCommand::Cancel)
        .await
        .expect("publish cancel control command");

    assert!(
        wait_for_condition(Duration::from_secs(5), || worker_saw_abort
            .load(Ordering::SeqCst))
        .await,
        "worker call_fn never observed abort after remote cancel publish"
    );

    let log_client = async_nats::connect(&url)
        .await
        .expect("connect nats client for session log polling");
    let log = NatsSessionLog::new(async_nats::jetstream::new(log_client), session_id.clone());
    let _cancel_entries = tokio::time::timeout(Duration::from_secs(5), async {
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

    let _turn_result = tokio::time::timeout(Duration::from_secs(5), run_turn)
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

    daemon.abort();
    let _ = daemon.await;
    let _ = child.kill();
    let _ = child.wait();
}

async fn registered_agent_provider(
    jetstream: &async_nats::jetstream::Context,
    config: &Config,
    agents: &[&str],
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
                if agents.iter().all(|agent| {
                    registrations
                        .iter()
                        .any(|(_, registration)| registration.server == *agent)
                }) {
                    let agent = agents.first().expect("at least one requested agent");
                    let key = registrations
                        .iter()
                        .find(|(_, registration)| registration.server == *agent)
                        .map(|(key, _)| key)
                        .expect("requested agent registration exists");
                    let instance_id = key
                        .strip_suffix(&format!(".____{agent}"))
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
        harnx_core::instance::InstanceId::from_string(instance_id.clone()),
        crate::nats_tool_provider::NatsInFlightCalls::default(),
        None,
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
    (result, child_session_id)
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
            .all(|tool| tool.name.starts_with(&format!("{agent}_session_"))));
    }

    let declarations = provider.declarations_for_use_tools(Some("*"));
    for agent in ["alpha", "beta"] {
        assert!(declarations
            .iter()
            .any(|tool| tool.name == format!("{agent}_{agent}_session_prompt")));
        let (result, _) = call_registered_agent(
            Arc::clone(&provider),
            format!("{agent}_{agent}_session_prompt"),
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
        .invoke("metis_session_new", json!({}), CancellationToken::new())
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
            "metis_session_prompt",
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
        registered_agent_provider(&jetstream, &seeded.parent_config, &["metis"]).await;
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
        .invoke("metis_session_new", json!({}), CancellationToken::new())
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
                "metis_session_prompt",
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
            "metis_session_load",
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
                    "metis_session_prompt",
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
                "metis_session_cancel",
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
            "metis_session_prompt",
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
            Duration::from_secs(2),
        ),
    )
    .await;
    let session_id = crate::nats_worker::new_remote_session_id();
    let activity_publisher = tokio::spawn({
        let url = url.clone();
        let session_id = session_id.clone();
        async move {
            let client = async_nats::connect(url)
                .await
                .expect("connect activity publisher");
            let subject = crate::nats_event_sink::events_subject(&session_id);
            for _ in 0..15 {
                tokio::time::sleep(Duration::from_millis(40)).await;
                let envelope = crate::nats_event_sink::AdvisoryEnvelope::new(
                    u64::MAX,
                    AgentEvent::Turn(harnx_core::event::TurnEvent::Started),
                );
                client
                    .publish(
                        subject.clone(),
                        envelope.to_bytes().expect("encode activity").into(),
                    )
                    .await
                    .expect("publish child activity");
                client.flush().await.expect("flush child activity");
            }
        }
    });
    let result = toolset
        .invoke(
            "metis_session_prompt",
            json!({ "message": "stay active", "session_id": session_id }),
            CancellationToken::new(),
        )
        .await
        .expect("activity must keep idle timeout from false-firing");
    assert_eq!(result["response"], "active child completed");
    activity_publisher.await.expect("join activity publisher");

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
    let thin = ThinClientSession::new(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(parent_session_id.clone()),
        },
        client.clone(),
        async_nats::jetstream::new(client.clone()),
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .expect("create parent thin-client session");
    let parent_result = tokio::time::timeout(
        Duration::from_secs(15),
        thin.run_turn(
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
async fn remote_session_activation_writes_session_index_record() {
    let _env_guard = env_lock().await;
    // Use an ISOLATED, self-provisioned NATS server (not the shared
    // HARNX_NATS_TEST_URL) so the thin-client `run_turn` isn't slowed by
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
    let store = ensure_index_bucket(&jetstream)
        .await
        .expect("ensure session index bucket");

    let daemon = spawn_metis_worker(&server_url);
    // Use the expected_session_id in the thin client config
    let round_trip =
        run_remote_round_trip_with_session_id(seeded.parent_config, expected_session_id.clone())
            .await;

    // Assert on the specific session_id we created, not just "first key in bucket"
    let record = get_record(&store, &expected_session_id)
        .await
        .expect("load session index record")
        .expect("remote session index record exists");
    assert_eq!(record.session_id, expected_session_id);
    assert_eq!(record.agent_name, "metis");
    assert!(record.last_activity > 0);

    daemon.abort();
    let _ = daemon.await;
    round_trip.expect("remote round trip must succeed");
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_session_renew_updates_last_activity_without_clobbering_header_fields() {
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
    let store = ensure_index_bucket(&jetstream)
        .await
        .expect("ensure session index bucket");

    // Arm the KV watcher BEFORE the session runs. `Store::watch` uses
    // DeliverPolicy::New (future updates only), so it must be established before
    // the initial index write and the lease-renewal refresh — otherwise both
    // Puts happen before the watcher exists and it waits forever.
    let record_key = session_index_key(&expected_session_id);
    let mut watcher = store
        .watch(record_key.clone())
        .await
        .expect("watch session index record");

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
    let mut first_record: Option<SessionIndexRecord> = None;
    let mut refreshed_record: Option<SessionIndexRecord> = None;
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
        let record: SessionIndexRecord =
            serde_json::from_slice(&entry.value).expect("deserialize watched session index record");
        if record.session_id != expected_session_id {
            continue;
        }
        match &first_record {
            None => first_record = Some(record),
            Some(first) if record.last_activity > first.last_activity => {
                refreshed_record = Some(record);
            }
            _ => {}
        }
    }

    let first_record = first_record.expect("activation should write initial session index record");
    let refreshed_record = refreshed_record.expect("renew should refresh last_activity");
    assert!(refreshed_record.last_activity > first_record.last_activity);
    // Renewal must refresh only last_activity, never clobber the header fields.
    assert_eq!(refreshed_record.agent_name, first_record.agent_name);
    assert_eq!(refreshed_record.working_dir, first_record.working_dir);
    assert_eq!(refreshed_record.git_branch, first_record.git_branch);
    assert_eq!(refreshed_record.git_remote, first_record.git_remote);

    daemon.abort();
    let _ = daemon.await;
    round_trip
        .await
        .expect("round trip task join")
        .expect("remote round trip must succeed");
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
    let log = NatsSessionLog::new(jetstream, session_id.clone());
    seed_remote_dispatch_session_log(&log, &session_id)
        .await
        .expect("seed remote session log");
    tokio::fs::create_dir_all(seeded.config_dir().join("sessions"))
        .await
        .expect("create session dir");
    let session_log_path = seeded
        .config_dir()
        .join("sessions")
        .join(format!("{session_id}.md"));
    tokio::fs::write(&session_log_path, "type: header\n")
        .await
        .expect("write stub session file");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    delete_remote_message_range(&global_config, 3, 3, &abort)
        .await
        .expect("delete display index 3");

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
                (4, 4),
                "display index 3 must target JetStream seq 4"
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
    let log = NatsSessionLog::new(jetstream, session_id.clone());
    seed_remote_dispatch_session_log(&log, &session_id)
        .await
        .expect("seed remote session log");
    tokio::fs::create_dir_all(seeded.config_dir().join("sessions"))
        .await
        .expect("create session dir");
    let session_log_path = seeded
        .config_dir()
        .join("sessions")
        .join(format!("{session_id}.md"));
    tokio::fs::write(&session_log_path, "type: header\n")
        .await
        .expect("write stub session file");

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

    edit_remote_message_range(&global_config, 3, 3, &abort)
        .await
        .expect("edit display index 3");

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
                (4, 4),
                "display index 3 must target JetStream seq 4"
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

    delete_remote_message_range(&global_config, 2, 2, &abort)
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
                (3, 3),
                "assistant message must map to physical seq 3"
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
async fn remote_delete_rejects_protected_rows() {
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
    let log = NatsSessionLog::new(jetstream, session_id.clone());
    let legacy_user_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some(uuid::Uuid::new_v4().to_string()),
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("legacy prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await
        .expect("append legacy user message");
    assert_eq!(legacy_user_seq, 1, "fixture should start headerless");

    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));
    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("worker should migrate headerless session");

    let mut parent_config = seeded.parent_config;
    parent_config.set_remote_agent("metis".to_string(), "local".to_string());
    parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(parent_config));
    let abort = harnx_core::abort::create_abort_signal();

    let err = delete_remote_message_range(&global_config, 0, 0, &abort)
        .await
        .expect_err("header row must stay protected");
    assert_eq!(
        err.to_string(),
        "Cannot edit or delete the session header (sequence 0)"
    );

    let err = delete_remote_message_range(&global_config, 0, 0, &abort)
        .await
        .expect_err("header row must stay protected");
    assert_eq!(
        err.to_string(),
        "Cannot edit or delete the session header (sequence 0)"
    );

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

    rewind_remote_session(&global_config, 1, &abort)
        .await
        .expect("rewind to first post-header row");

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
            assert_eq!(
                *after_seq, 2,
                "logical row 1 must map to physical seq 2 (legacy Rewind path)"
            );
        }
        SessionLogEntry::EditEntries {
            from,
            to,
            replacements,
        } => {
            assert!(*from > 0, "edit entries must target post-header sequences");
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
        &ThinClientSession::from_global_config(
            crate::ThinClientConfig {
                cluster: "local".to_string(),
                agent: "metis".to_string(),
                session_id: Some(session_id.clone()),
            },
            &global_config,
            abort.clone(),
        )
        .await
        .expect("load thin session"),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_remote_transcript_for_render_prerenders_logical_rows() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let session_id = crate::nats_worker::new_remote_session_id();
    let worker =
        spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("stub remote reply over nats"));

    run_remote_round_trip_with_session_id(seeded.parent_config.clone(), session_id.clone())
        .await
        .expect("worker should build realistic migrated session");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();
    let thin = ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id.clone()),
        },
        &global_config,
        abort,
    )
    .await
    .expect("load thin session");

    let transcript = load_remote_transcript_for_render(&thin)
        .await
        .expect("load transcript state");
    assert!(
        transcript.compressed_messages.is_empty(),
        "worker-migrated headerless fixture should have empty compacted prefix"
    );
    let row_seqs: Vec<usize> = transcript
        .messages
        .iter()
        .filter_map(|message| message.log_seq)
        .collect();
    assert_eq!(row_seqs, vec![1, 2]);
    let row_texts: Vec<(harnx_core::message::MessageRole, String)> = transcript
        .messages
        .iter()
        .map(|message| (message.role, message.content.to_text()))
        .collect();
    assert_eq!(
        row_texts,
        vec![
            (
                harnx_core::message::MessageRole::User,
                "delegate over nats".to_string(),
            ),
            (
                harnx_core::message::MessageRole::Assistant,
                "stub remote reply over nats".to_string(),
            ),
        ]
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_remote_transcript_for_render_keeps_tool_rows_and_compressed_prefix() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
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

    let tool_call = ToolCall::new(
        "fs_read".to_string(),
        json!({"path": "README.md"}),
        Some("tool-1".to_string()),
        None,
    );
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "calling tool".to_string(),
        thought: Some("tool thought".to_string()),
        calls: vec![tool_call.clone()],
        timestamp: None,
        fence_token: None,
    })
    .await
    .expect("append tool calls");
    log.append_event_async(&SessionLogEntry::ToolResults {
        results: vec![ToolOutput {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            output: json!({"ok": true}),
            markdown: None,
            content: vec![],
            switch_agent: None,
        }],
        timestamp: None,
    })
    .await
    .expect("append tool results");
    log.append_event_async(&SessionLogEntry::Compress {
        prompt: "summary prompt".to_string(),
    })
    .await
    .expect("append compress");
    log.append_event_async(&SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("after compress user".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await
    .expect("append post-compress user");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let thin = ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id.clone()),
        },
        &global_config,
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .expect("load thin session");

    let transcript = load_remote_transcript_for_render(&thin)
        .await
        .expect("load transcript state");

    assert!(
        !transcript.compressed_messages.is_empty(),
        "compressed_messages must be populated after remote Compress"
    );
    assert!(
        transcript
            .compressed_messages
            .iter()
            .any(|message| message.role == harnx_core::message::MessageRole::Tool),
        "compressed prefix must retain assembled tool message rows"
    );
    assert!(
        transcript
            .messages
            .iter()
            .all(|message| message.role != harnx_core::message::MessageRole::System),
        "active transcript must not include a synthetic System message after Compress"
    );
    assert_eq!(
        transcript.compaction_summary.as_deref(),
        Some("summary prompt"),
        "compaction summary must carry the summary text after Compress"
    );

    let active_rows: Vec<(harnx_core::message::MessageRole, String, Option<usize>)> = transcript
        .messages
        .iter()
        .map(|message| (message.role, message.content.to_text(), message.log_seq))
        .collect();
    assert_eq!(
        active_rows,
        vec![(
            harnx_core::message::MessageRole::User,
            "after compress user".to_string(),
            Some(0),
        ),]
    );

    worker.abort();
    let _ = worker.await;
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_edit_preserves_header_in_migrated_session() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
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

    // Edit the first user message (logical index 1) which shares physical seq with header
    edit_remote_message_range(&global_config, 1, 1, &abort)
        .await
        .expect("edit older user message");

    let thin = ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id.clone()),
        },
        &global_config,
        abort,
    )
    .await
    .expect("load thin session");
    let state = load_remote_session_for_render(&thin)
        .await
        .expect("load remote render state");

    // CRITICAL: header must survive the edit
    let header_count = state
        .logical_entries
        .iter()
        .filter(|entry| matches!(entry, SessionLogEntry::Header { .. }))
        .count();
    assert_eq!(
        header_count, 1,
        "header must survive edit of first user message in migrated session (shared-seq bug fix)"
    );

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_delete_after_older_edit_deletes_exact_late_range() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
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

    edit_remote_message_range(&global_config, 1, 1, &abort)
        .await
        .expect("edit older user message");
    delete_remote_message_range(&global_config, 3, 4, &abort)
        .await
        .expect("delete later logical range");

    let thin = ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id.clone()),
        },
        &global_config,
        abort,
    )
    .await
    .expect("load thin session");
    let state = load_remote_session_for_render(&thin)
        .await
        .expect("load remote render state");
    // Assert header survives the edit (key coverage for shared-seq bug fix)
    let header_count = state
        .logical_entries
        .iter()
        .filter(|entry| matches!(entry, SessionLogEntry::Header { .. }))
        .count();
    assert_eq!(
        header_count, 1,
        "header must survive edit of first user message in migrated session"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_rewind_after_older_edit_preserves_correct_logical_prefix() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
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

    edit_remote_message_range(&global_config, 1, 1, &abort)
        .await
        .expect("edit older user message");
    rewind_remote_session(&global_config, 2, &abort)
        .await
        .expect("rewind logical suffix");

    let thin = ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id.clone()),
        },
        &global_config,
        abort,
    )
    .await
    .expect("load thin session");
    let state = load_remote_session_for_render(&thin)
        .await
        .expect("load remote render state");
    // Assert header survives the edit (key coverage for shared-seq bug fix)
    let header_count = state
        .logical_entries
        .iter()
        .filter(|entry| matches!(entry, SessionLogEntry::Header { .. }))
        .count();
    assert_eq!(
        header_count, 1,
        "header must survive edit of first user message in migrated session"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_delete_command_routes_to_exact_set_mutations() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
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

    edit_remote_message_range(&global_config, 1, 1, &abort)
        .await
        .expect("edit older user message");
    crate::commands::run_command(&global_config, abort.clone(), ".delete message 3-4")
        .await
        .expect("remote delete command succeeds");

    let thin = ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id.clone()),
        },
        &global_config,
        abort.clone(),
    )
    .await
    .expect("load thin session");
    let state = load_remote_session_for_render(&thin)
        .await
        .expect("load remote render state");
    // Assert header survives the edit (key coverage for shared-seq bug fix)
    let header_count = state
        .logical_entries
        .iter()
        .filter(|entry| matches!(entry, SessionLogEntry::Header { .. }))
        .count();
    assert_eq!(
        header_count, 1,
        "header must survive edit of first user message in migrated session"
    );
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
    let header_present = effective
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Header { .. }));
    assert!(
        header_present,
        "header must be present in reconstructed log after edit+delete"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_rewind_command_routes_to_exact_suffix_deletions() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
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

    edit_remote_message_range(&global_config, 1, 1, &abort)
        .await
        .expect("edit older user message");
    crate::commands::run_command(&global_config, abort.clone(), ".rewind 2")
        .await
        .expect("remote rewind command succeeds");

    let thin = ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id.clone()),
        },
        &global_config,
        abort.clone(),
    )
    .await
    .expect("load thin session");
    let state = load_remote_session_for_render(&thin)
        .await
        .expect("load remote render state");
    // Assert header survives the edit (key coverage for shared-seq bug fix)
    let header_count = state
        .logical_entries
        .iter()
        .filter(|entry| matches!(entry, SessionLogEntry::Header { .. }))
        .count();
    assert_eq!(
        header_count, 1,
        "header must survive edit of first user message in migrated session"
    );
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
    let header_present = effective
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Header { .. }));
    assert!(
        header_present,
        "header must be present in reconstructed log after edit+rewind"
    );
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

/// Regression (PR #956 F-B): a headerless remote session with MULTIPLE leading
/// user messages before worker activation. The worker migration clones all
/// leading users into ONE header-insert EditEntries, so Header + every leading
/// user share the SAME physical JetStream seq. The resume renumbering must give
/// each shared-seq user row its OWN distinct logical index ([1,2,3,...]) rather
/// than collapsing them to a single index.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_remote_transcript_multi_leading_user_rows_are_distinct() {
    use harnx_core::message::{MessageContent, MessageRole};
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let session_id = crate::nats_worker::new_remote_session_id();

    // Seed two leading user messages directly to the durable log BEFORE the
    // worker activates, mirroring a thin client that appended several prompts
    // before the first worker turn.
    let jetstream = seeded
        .parent_config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
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
        .expect("worker migrates multi-leading-user session");

    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(seeded.parent_config));
    let abort = harnx_core::abort::create_abort_signal();
    let thin = ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id.clone()),
        },
        &global_config,
        abort,
    )
    .await
    .expect("load thin session");

    let transcript = load_remote_transcript_for_render(&thin)
        .await
        .expect("load transcript state");

    let row_seqs: Vec<usize> = transcript
        .messages
        .iter()
        .filter_map(|message| message.log_seq)
        .collect();
    // Header = logical 0 (no row). Rows: leading one=1, leading two=2,
    // "delegate over nats"=3, assistant=4. On the pre-fix code these collapsed
    // to [3,3,3,4]. Assert distinct + contiguous starting at 1.
    assert_eq!(
        row_seqs,
        vec![1, 2, 3, 4],
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
