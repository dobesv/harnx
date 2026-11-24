//! Durable-approval coverage across a NATS worker: acknowledgement recovery,
//! decision fencing and fixed-prompt follow boundaries. The frontend-side
//! bridge still handles middleware confirmations, but no new process-hook
//! approvals are created. These tests seed durable `HitlApproval*` entries
//! directly to cover decisions from sessions saved before removal.

#[path = "nats_tool_confirmation/ack_recovery.rs"]
mod ack_recovery;
use crate::common;
use harnx_runtime::nats_session_log::NatsSessionLog;
#[path = "nats_tool_confirmation/fencing.rs"]
mod fencing;
use crate::generation;
#[path = "nats_tool_confirmation/follow_boundary.rs"]
mod follow_boundary;

use anyhow::{Context, Result};
use harnx_core::{
    message::MessageContent, require_nextest, session::SessionLogEntry, tool::ToolCall,
};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::Config,
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session::{NatsSession, NatsSessionConfig},
    nats_session_metadata::SessionInitializer,
    nats_worker::{run_worker_daemon, WorkerDaemonConfig},
    utils::create_abort_signal,
};
use parking_lot::Mutex;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const SOURCE_SESSION_ID: &str = "hook-approval-source";
const TARGET_SESSION_ID: &str = "hook-approval-target";

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

struct TestEnvironment {
    config_dir: PathBuf,
    _root: tempfile::TempDir,
    _guards: Vec<EnvVarGuard>,
}

impl TestEnvironment {
    fn new(server_url: &str) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        for dir in [
            config_dir.clone(),
            config_dir.join("agents"),
            config_dir.join("clients"),
            config_dir.join("nats_servers"),
            data_dir.clone(),
            state_dir.clone(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        write_test_config(&config_dir, server_url)?;
        let guards = vec![
            EnvVarGuard::set_path("HARNX_CONFIG_DIR", &config_dir),
            EnvVarGuard::set_path("HARNX_DATA_DIR", &data_dir),
            EnvVarGuard::set_path("HARNX_STATE_DIR", &state_dir),
            EnvVarGuard::set_path("HARNX_NATS_URL", Path::new(server_url)),
            EnvVarGuard::set_path("HARNX_NATS_TOKEN", Path::new("test-token")),
        ];
        Ok(Self {
            config_dir,
            _root: root,
            _guards: guards,
        })
    }

    async fn load(
        &self,
    ) -> Result<(
        harnx_runtime::config::GlobalConfig,
        async_nats::Client,
        async_nats::jetstream::Context,
    )> {
        let config = Arc::new(harnx_runtime::config::ConfigLock::new(
            Config::load_from_file(&self.config_dir.join("config.yaml"))?,
        ));
        let cfg = config.read().clone();
        let client = cfg.nats_client("local").await?;
        let jetstream = cfg.nats_jetstream("local").await?;
        Ok((config, client, jetstream))
    }
}

struct ConfirmationHarness {
    source: NatsSession,
    jetstream: async_nats::jetstream::Context,
    daemon: tokio::task::JoinHandle<Result<()>>,
    _server: common::NatsServerHandle,
    _environment: TestEnvironment,
}

impl ConfirmationHarness {}

impl Drop for ConfirmationHarness {
    fn drop(&mut self) {
        self.daemon.abort();
    }
}

async fn start_ready_daemon(
    config: harnx_runtime::config::GlobalConfig,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
    client: &async_nats::Client,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    use futures_util::StreamExt;

    // Subscribe before spawning so even immediate readiness cannot be missed.
    let mut ready = client
        .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
        .await?;
    client.flush().await?;
    let mut daemon = tokio::spawn(run_worker_daemon(
        config,
        WorkerDaemonConfig::managing("local", "worker-tool-confirmation"),
        Some(call_fn),
        None,
    ));
    let outcome = tokio::select! {
        message = ready.next() => message.context("worker readiness subscription closed").map(|_| ()),
        result = &mut daemon => Err(anyhow::anyhow!("worker exited before readiness: {result:?}")),
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            Err(anyhow::anyhow!("worker daemon did not become ready within 30s"))
        }
    };
    if let Err(error) = outcome {
        daemon.abort();
        return Err(error);
    }
    Ok(daemon)
}

fn write_test_config(config_dir: &Path, server_url: &str) -> Result<()> {
    std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n")?;
    std::fs::write(
        config_dir.join("nats_servers/local.yaml"),
        format!("url: {server_url}\n"),
    )?;
    std::fs::write(
        config_dir.join("clients/openai.yaml"),
        "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
    )?;
    std::fs::write(
        config_dir.join("agents/approval-gated.md"),
        "---\nmodel: openai:test-model\nuse_tools:\n- target_session_handoff\n- time_get_current_time\n---\nApproval-gated agent instructions\n",
    )?;
    std::fs::write(
        config_dir.join("agents/target.md"),
        "---\nmodel: openai:test-model\n---\nTarget agent instructions\n",
    )?;
    Ok(())
}

fn source_key() -> String {
    harnx_core::session_identity::session_key(Some("approval-gated"), SOURCE_SESSION_ID)
}
