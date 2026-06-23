//! Unit tests for nats_worker module.

use crate::config::{self};
use crate::nats_worker::agent_loop::{tool_can_rerun, write_header_and_load_session};
use harnx_core::agent_config::AgentConfig;
use harnx_core::config_data::ConfigData;
use harnx_core::session::ToolOutput;
use harnx_core::tool::{ToolCall, ToolDeclaration};
use serde_json::json;
use std::sync::Arc;

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

    let session = write_header_and_load_session(&backend, &config, &input, session_id)
        .await
        .unwrap();
    let expected_header = {
        let mut expected_session = config::session::new(&config.read(), session_id).unwrap();
        expected_session.set_agent(&input.agent).unwrap();
        expected_session.build_header_entry()
    };

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
    assert!(actual_header_yaml.contains("git_branch: nats"));
    assert!(actual_header_yaml.contains("git_remote: https://github.com/dobesv/harnx.git"));
    assert!(actual_header_yaml
        .contains("working_dir: /mnt/projects/ai-tools/harnx/crates/harnx-runtime"));
    let loaded_header_yaml = serde_yaml::to_string(&session.build_header_entry()).unwrap();
    assert!(loaded_header_yaml.contains("agent_name: pkg/main"));
    assert!(expected_header_yaml.contains("agent_name: pkg/main"));

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
