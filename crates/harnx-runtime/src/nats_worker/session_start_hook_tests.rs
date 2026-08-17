//! End-to-end coverage for worker-dispatched SessionStart hooks.

use super::tests::{
    env_lock, fixed_prompt_call_fn, run_remote_round_trip_with_session_id_and_sink,
    seed_remote_config, spawn_metis_worker_with_hooks, spawn_test_nats, wait_for_condition,
    NoopEventSink, TestEnvGuard,
};
use crate::config::{HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV, LOCAL_CLUSTER_KEY};
use crate::nats_worker::{new_remote_session_id, WorkerDaemonConfig};
use harnx_core::hooks::{HookConfig, HooksConfig};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// Resolve the hook-server binary next to the test executable, building it if a
/// package-scoped test run has not produced it yet.
fn hook_server_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("resolve test executable");
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) {
        "harnx-claude-compatible-hook-server.exe"
    } else {
        "harnx-claude-compatible-hook-server"
    });
    if path.is_file() {
        return path;
    }

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("resolve workspace root")
        .to_path_buf();
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "-p", "harnx-claude-compatible-hook-server"])
        .current_dir(workspace)
        .status()
        .expect("build harnx-claude-compatible-hook-server for hook dispatch test");
    assert!(status.success(), "building the hook server failed");
    assert!(
        path.is_file(),
        "hook server not found at {}",
        path.display()
    );
    path
}

/// A worker-launched SessionStart hook that appends one line per invocation.
fn session_start_marker_hook(marker: &Path) -> HooksConfig {
    HooksConfig {
        max_resume: None,
        entries: vec![HookConfig {
            command: format!(
                "{} --event SessionStart --timeout 30 -- {} 'echo fired >> \"{}\"'",
                hook_server_binary().display(),
                if cfg!(windows) { "cmd /C" } else { "sh -c" },
                marker.display()
            ),
            status_message: None,
            async_hook: None,
            package_dir: None,
        }],
    }
}

fn marker_line_count(marker: &Path) -> usize {
    std::fs::read_to_string(marker)
        .map(|contents| contents.lines().count())
        .unwrap_or(0)
}

// This subprocess timing assertion is covered on Unix; Windows CI cannot
// observe the hook before its bounded worker shutdown deadline.
#[cfg_attr(
    windows,
    ignore = "worker hook observation exceeds deadline on Windows"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_remote_session_fires_session_start_hook_exactly_once() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let marker_dir = tempfile::tempdir().expect("marker temp dir");
    let marker = marker_dir.path().join("session-start.log");
    let seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    // The worker resolves the local NATS server from the environment when it
    // launches hook servers.
    let _nats_url = TestEnvGuard::new(HARNX_NATS_URL_ENV, &url);
    let _nats_token = TestEnvGuard::new(HARNX_NATS_TOKEN_ENV, "session-start-token");

    // Hook servers only launch when the worker manages its own, which
    // resolves its connection from the environment handoff set above.
    let worker = spawn_metis_worker_with_hooks(
        &url,
        fixed_prompt_call_fn("stub remote reply over nats"),
        WorkerDaemonConfig::managing(LOCAL_CLUSTER_KEY, "worker-metis"),
        Some(session_start_marker_hook(&marker)),
    );

    let session_id = new_remote_session_id();
    run_remote_round_trip_with_session_id_and_sink(
        seeded.parent_config.clone(),
        session_id.clone(),
        Arc::new(NoopEventSink),
        LOCAL_CLUSTER_KEY,
    )
    .await
    .expect("first turn of a brand-new remote session");

    assert!(
        wait_for_condition(Duration::from_secs(10), || marker_line_count(&marker) > 0).await,
        "SessionStart hook must run when the worker creates a session"
    );

    // A second turn re-activates the same session. SessionStart belongs to the
    // session, not the turn, so it must not fire again.
    run_remote_round_trip_with_session_id_and_sink(
        seeded.parent_config,
        session_id,
        Arc::new(NoopEventSink),
        LOCAL_CLUSTER_KEY,
    )
    .await
    .expect("second turn of the same remote session");
    assert_eq!(
        marker_line_count(&marker),
        1,
        "SessionStart must fire once per session, not once per activation"
    );

    worker.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), worker).await;
    let _ = child.kill();
    let _ = child.wait();
}
