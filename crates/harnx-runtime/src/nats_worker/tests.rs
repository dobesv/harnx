//! Unit tests for nats_worker module.

use crate::config::{self, Config};
use crate::nats_worker::agent_loop::{tool_can_rerun, write_header_and_load_session};
use crate::nats_worker::run_worker_daemon;
use anyhow::Context;
use harnx_core::agent_config::AgentConfig;
use harnx_core::config_data::ConfigData;
use harnx_core::event::{AgentEvent, AgentEventSink};
use harnx_core::session::ToolOutput;
use harnx_core::tool::{ToolCall, ToolDeclaration};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};

/// Spawn a local JetStream-enabled nats-server on a free port with an isolated
/// temp store dir, returning the connect URL, the child process, and the temp
/// dir guard. Using a free port + per-run store dir avoids cross-run state
/// bleed (JetStream KV/lease buckets) and port collisions that make tests flaky
/// when run repeatedly or in parallel. Returns `None` if nats-server is absent.
async fn spawn_test_nats() -> Option<(String, std::process::Child, tempfile::TempDir)> {
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

static CWD_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_MUTEX.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct TestEnvGuard {
    prev: Option<std::ffi::OsString>,
}

impl TestEnvGuard {
    fn new(key: &str, value: &Path) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        let _ = key;
        Self { prev }
    }
}

impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => unsafe { std::env::set_var("HARNX_CONFIG_DIR", value) },
            None => unsafe { std::env::remove_var("HARNX_CONFIG_DIR") },
        }
    }
}

fn load_config_via_internal_pipeline(config_path: &Path) -> Config {
    let prev = std::env::var_os("HARNX_CONFIG_DIR");
    let config_dir = config_path
        .parent()
        .expect("config path must have parent directory");
    let _config_guard = TestEnvGuard::new("HARNX_CONFIG_DIR", config_dir);
    let mut config = Config::load_from_file(config_path).unwrap();
    // Initialize acp_manager from the auto-registered ACP servers; without this
    // the delegation tool family is never materialized and only the handoff
    // path contributes to tool_declarations_for_use_tools. Mirrors the config
    // tests' `remote_use_tools_selectors_match_full_sanitized_family`.
    config.reinit_managers_for_agent(None);
    drop(_config_guard);
    match prev {
        Some(value) => unsafe { std::env::set_var("HARNX_CONFIG_DIR", value) },
        None => unsafe { std::env::remove_var("HARNX_CONFIG_DIR") },
    }
    config
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
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let temp_repo = create_test_git_repo();
    let temp_repo_path = temp_repo.path().to_path_buf();
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
        let session = write_header_and_load_session(&backend, &config, &input, session_id)
            .await
            .unwrap();
        let expected_header = {
            let mut expected_session = config::session::new(&config.read(), session_id).unwrap();
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
    assert!(expected_header_yaml.contains("git_branch: test-branch"));
    assert!(expected_header_yaml.contains("git_remote: https://example.com/test/repo.git"));
    assert!(expected_header_yaml.contains(&expected_working_dir));
    assert_eq!(
        actual_header_yaml, expected_header_yaml,
        "remote header must match locally built header"
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

struct NoopEventSink;

impl AgentEventSink for NoopEventSink {
    fn emit(&self, _event: AgentEvent, _source: Option<harnx_core::event::AgentSource>) {}
}

fn fixed_prompt_call_fn(reply: &'static str) -> crate::agent_loop::AgentCallFn {
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

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_agent_tool_family_and_nats_call_and_return_round_trip() {
    use std::fs;

    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let _env_guard = env_lock();
    let temp = tempfile::TempDir::new().unwrap();
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", temp.path());
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

    let mut parent_config = load_config_via_internal_pipeline(&temp.path().join("config.yaml"));

    let expected_tools = [
        "metis__at__local_session_cancel",
        "metis__at__local_session_handoff",
        "metis__at__local_session_load",
        "metis__at__local_session_new",
        "metis__at__local_session_prompt",
    ];

    let mut whitelisted_names: Vec<String> = parent_config
        .tool_declarations_for_use_tools(Some("metis@local"), None)
        .0
        .into_iter()
        .map(|tool| tool.name)
        .filter(|name| name.starts_with("metis__at__local_session_"))
        .collect();
    whitelisted_names.sort();
    assert_eq!(
        whitelisted_names,
        expected_tools
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    );

    let wildcard_tools = parent_config
        .tool_declarations_for_use_tools(Some("*"), None)
        .0;
    let mut wildcard_names: Vec<String> = wildcard_tools
        .iter()
        .map(|tool| tool.name.clone())
        .filter(|name| name.starts_with("metis__at__local_session_"))
        .collect();
    wildcard_names.sort();
    assert_eq!(
        wildcard_names,
        expected_tools
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    );

    let prompt_tool = wildcard_tools
        .iter()
        .find(|tool| tool.name == "metis__at__local_session_prompt")
        .expect("prompt tool must exist");
    let prompt_description = &prompt_tool.description;
    assert!(
        prompt_description.contains("Remote planner over NATS"),
        "prompt tool description must include seeded catalog description: {prompt_description}"
    );
    assert!(
        prompt_description.ends_with("Remote planner over NATS"),
        "prompt tool description must end with seeded catalog description: {prompt_description}"
    );

    let worker_agent = AgentConfig::from_markdown("metis", "stub worker prompt").unwrap();
    let worker_config = Config {
        data: harnx_core::config_data::ConfigData {
            model_id: "test:test-model".to_string(),
            ..Default::default()
        },
        agent: Some(crate::config::Agent::new(worker_agent)),
        nats_servers: vec![config::NatsServerConfig {
            name: "local".to_string(),
            url: url.clone(),
            token: None,
            tls: Some(false),
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: Vec::new(),
        }],
        ..Default::default()
    };
    let worker_config = Arc::new(parking_lot::RwLock::new(worker_config));
    let daemon = tokio::spawn({
        let worker_config = Arc::clone(&worker_config);
        async move {
            run_worker_daemon(
                worker_config,
                crate::nats_worker::WorkerDaemonConfig::new("local", "worker-metis"),
                Some(fixed_prompt_call_fn("stub remote reply over nats")),
            )
            .await
        }
    });

    let test_result: anyhow::Result<()> = async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let parent_session = crate::config::session::new(&parent_config, "parent-nats-roundtrip")?;
        parent_config.session = Some(parent_session);
        let parent_global_config = Arc::new(parking_lot::RwLock::new(parent_config));
        let abort_signal = harnx_core::abort::create_abort_signal();
        let thin_cfg = crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(crate::nats_worker::new_remote_session_id()),
        };
        let thin = crate::ThinClientSession::from_global_config(
            thin_cfg,
            &parent_global_config,
            abort_signal,
        )
        .await?;

        let turn_result = thin
            .run_turn("delegate over nats", Arc::new(NoopEventSink), None)
            .await?;
        let reply = turn_result
            .response
            .context("thin client turn must return final assistant response")?;
        anyhow::ensure!(
            reply.contains("stub remote reply over nats"),
            "expected reply to contain stub remote reply, got: {reply}"
        );
        Ok(())
    }
    .await;

    daemon.abort();
    let _ = daemon.await;
    let _ = child.kill();
    let _ = child.wait();
    test_result.unwrap()
}
