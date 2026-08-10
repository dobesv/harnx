//! Refcount and linger rules for `ServerReconciler`, pinned against a fake
//! launcher so they need no broker and no child processes.
//!
//! Sibling files split by topic: `tool_reconciler_race.rs` (teardown/start
//! races and concurrency) and `tool_reconciler_e2e.rs` (a real managing
//! worker against a real broker and real server binaries).

use async_trait::async_trait;
use harnx_runtime::config::ToolServerConfig;
use harnx_runtime::nats_worker::server_reconciler::{ServerLauncher, ServerReconciler};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct FakeLauncher {
    started: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
}

#[async_trait]
impl ServerLauncher for FakeLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        self.started.lock().unwrap().push(server.name.clone());
        Ok(())
    }
    async fn stop(&self, config_name: &str) {
        self.stopped.lock().unwrap().push(config_name.to_string());
    }
}

/// A launcher whose `start` always fails, for the "one bad server must not
/// cost the session the others" rule.
#[derive(Default)]
struct FailingLauncher {
    started_attempts: Mutex<Vec<String>>,
}

#[async_trait]
impl ServerLauncher for FailingLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        self.started_attempts
            .lock()
            .unwrap()
            .push(server.name.clone());
        anyhow::bail!("boom")
    }
    async fn stop(&self, _config_name: &str) {}
}

fn tool_server(name: &str) -> ToolServerConfig {
    // `ToolServerConfig` does not derive `Default`, so every field is
    // explicit. Same shape as `fn tool_server` in the `daemon.rs` test module
    // and this file's siblings — keep them in sync.
    ToolServerConfig {
        name: name.to_string(),
        command: format!("harnx-{name}-server"),
        args: Vec::new(),
        env: Default::default(),
        enabled: true,
        description: None,
        package: None,
        hooks: None,
    }
}

#[tokio::test]
async fn a_server_two_sessions_share_stays_up_until_both_end() {
    harnx_core::require_nextest();
    let launcher = Arc::new(FakeLauncher::default());
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::ZERO);

    reconciler
        .session_started("s1", vec![tool_server("time")])
        .await;
    reconciler
        .session_started("s2", vec![tool_server("time")])
        .await;
    assert_eq!(reconciler.running().await, vec!["time"]);
    assert_eq!(
        launcher.started.lock().unwrap().len(),
        1,
        "the second session must reuse the running server, not start a second one"
    );

    reconciler.session_ended("s1").await;
    assert_eq!(
        reconciler.running().await,
        vec!["time"],
        "s2 still needs it"
    );
    assert!(launcher.stopped.lock().unwrap().is_empty());

    reconciler.session_ended("s2").await;
    assert!(reconciler.running().await.is_empty());
    assert_eq!(launcher.stopped.lock().unwrap().as_slice(), ["time"]);
}

#[tokio::test]
async fn linger_keeps_a_server_up_across_back_to_back_sessions() {
    harnx_core::require_nextest();
    let launcher = Arc::new(FakeLauncher::default());
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::from_secs(30));

    reconciler
        .session_started("s1", vec![tool_server("time")])
        .await;
    reconciler.session_ended("s1").await;
    assert_eq!(
        reconciler.running().await,
        vec!["time"],
        "within the linger window the process should be reused, not restarted"
    );

    reconciler
        .session_started("s2", vec![tool_server("time")])
        .await;
    assert_eq!(
        launcher.started.lock().unwrap().len(),
        1,
        "no restart should have happened"
    );
    assert!(launcher.stopped.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unrelated_servers_do_not_share_a_refcount() {
    harnx_core::require_nextest();
    let launcher = Arc::new(FakeLauncher::default());
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::ZERO);

    reconciler
        .session_started("s1", vec![tool_server("time"), tool_server("plans")])
        .await;
    assert_eq!(reconciler.running().await, vec!["plans", "time"]);

    reconciler.session_ended("s1").await;
    assert!(
        reconciler.running().await.is_empty(),
        "both servers lost their only user"
    );
    let mut stopped = launcher.stopped.lock().unwrap().clone();
    stopped.sort();
    assert_eq!(stopped, vec!["plans", "time"]);
}

#[tokio::test]
async fn a_server_that_fails_to_start_is_not_counted_as_running() {
    harnx_core::require_nextest();
    let launcher = Arc::new(FailingLauncher::default());
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::ZERO);

    reconciler
        .session_started("s1", vec![tool_server("broken")])
        .await;

    assert!(
        reconciler.running().await.is_empty(),
        "a server that failed to start must not be tracked as running"
    );
    assert_eq!(
        launcher.started_attempts.lock().unwrap().as_slice(),
        ["broken"]
    );
}
