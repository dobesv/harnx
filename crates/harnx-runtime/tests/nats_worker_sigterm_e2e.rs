//! Real-process SIGTERM coverage for worker failover.

#![cfg(unix)]

mod common;
#[allow(dead_code)]
#[path = "common/worker.rs"]
mod worker;

use anyhow::{Context, Result};
use async_nats::jetstream::{consumer::PullConsumer, kv::Operation};
use futures_util::StreamExt;
use harnx_core::{event::NullSink, instance::ServerScope, session::SessionLogEntry};
use harnx_runtime::{
    nats_lease::{lease_holder_in, open_lease_bucket, NatsLeaseConfig},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionInitializer, SessionOverrides},
    utils::create_abort_signal,
    NatsSession, NatsSessionConfig, NatsTurnResult, RunTurnOptions, SessionActivationRoute,
};
use harnx_toolset::{CancellationGuarantee, ToolInvokeError, ToolSpec, Toolset};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

const CLUSTER: &str = "sigterm";
const CI_TIMEOUT: Duration = Duration::from_secs(60);
const FAILOVER_ORPHAN_TIMEOUT: Duration = Duration::from_secs(60);

struct ChildGuard(Child);

impl ChildGuard {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    async fn wait_for_exit(&mut self) -> Result<std::process::ExitStatus> {
        tokio::time::timeout(CI_TIMEOUT, async {
            loop {
                if let Some(status) = self.0.try_wait()? {
                    return Ok::<_, std::io::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("worker did not exit after SIGTERM")?
        .context("poll worker exit")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Copy)]
enum ModelScript {
    BlockThenComplete,
    ToolThenComplete,
}

struct MockModel {
    api_base: String,
    first_started: Arc<Notify>,
    complete_replacement: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl MockModel {
    async fn start(script: ModelScript) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let first_started = Arc::new(Notify::new());
        let complete_replacement = Arc::new(Notify::new());
        let first_notice = Arc::clone(&first_started);
        let replacement_release = Arc::clone(&complete_replacement);
        let task = tokio::spawn(async move {
            for request_index in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept mock model request");
                read_http_request(&mut socket)
                    .await
                    .expect("read mock model request");
                match (script, request_index) {
                    (ModelScript::BlockThenComplete, 0) => {
                        first_notice.notify_one();
                        let mut byte = [0_u8; 1];
                        while socket.read(&mut byte).await.unwrap_or(0) != 0 {}
                    }
                    (ModelScript::ToolThenComplete, 0) => {
                        first_notice.notify_one();
                        write_json_response(&mut socket, tool_call_response())
                            .await
                            .expect("write tool-call response");
                    }
                    (_, 1) => {
                        replacement_release.notified().await;
                        write_json_response(&mut socket, final_response())
                            .await
                            .expect("write final response");
                    }
                    _ => unreachable!(),
                }
            }
        });
        Ok(Self {
            api_base: format!("http://{address}/v1"),
            first_started,
            complete_replacement,
            task,
        })
    }
}

impl Drop for MockModel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct HeldTool {
    entered: Notify,
}

#[async_trait::async_trait]
impl Toolset for HeldTool {
    fn name(&self) -> &str {
        "held"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "wait".into(),
            description: "Remain in flight until its worker hands the turn off".into(),
            input_schema: json!({"type": "object"}),
            cancellation_guarantee: CancellationGuarantee::Cooperative,
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        tool: &str,
        _args: serde_json::Value,
        _cancel: CancellationToken,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        assert_eq!(tool, "wait");
        self.entered.notify_one();
        std::future::pending().await
    }
}

struct Fixture {
    _server: common::NatsServerHandle,
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    worker_binary: PathBuf,
    root: tempfile::TempDir,
    scope: ServerScope,
    held_tool: Arc<HeldTool>,
    _tool_server: AbortOnDropHandle<Result<()>>,
    _tool_env: [worker::EnvGuard; 3],
}

impl Fixture {
    async fn start() -> Result<Option<Self>> {
        harnx_core::require_nextest();
        let Some(worker_binary) = common::harnx_worker_binary() else {
            eprintln!("skipping worker SIGTERM e2e: harnx-worker binary not found");
            return Ok(None);
        };
        let Some(server) = common::spawn_nats_server().await? else {
            return Ok(None);
        };
        let client = async_nats::connect(server.url()).await?;
        let jetstream = async_nats::jetstream::new(client.clone());
        let root = tempfile::tempdir()?;
        create_isolated_dirs(root.path())?;
        let scope = ServerScope::new();
        let tool_env = worker::EnvGuard::tool_server_environment(&scope, server.url());
        let held_tool = Arc::new(HeldTool::default());
        let tool_server = worker::start_tool_server(
            &jetstream,
            client.clone(),
            scope.clone(),
            Arc::clone(&held_tool),
        )
        .await?;
        Ok(Some(Self {
            _server: server,
            client,
            jetstream,
            worker_binary,
            root,
            scope,
            held_tool,
            _tool_server: tool_server,
            _tool_env: tool_env,
        }))
    }

    fn configure_model(&self, api_base: &str) -> Result<()> {
        let config = self.root.path().join("config");
        std::fs::write(
            config.join("config.yaml"),
            "save: false\nstream: false\nclient: mock\nmodel: mock:test\n",
        )?;
        std::fs::write(
            config.join("clients/mock.yaml"),
            format!(
                "type: openai-compatible\nname: mock\napi_base: {api_base:?}\napi_key: test-key\nmodels:\n  - name: test\n    max_input_tokens: 32000\n    max_output_tokens: 1024\n"
            ),
        )?;
        std::fs::write(
            config.join(format!("nats_servers/{CLUSTER}.yaml")),
            format!("url: {:?}\n", self._server.url()),
        )?;
        Ok(())
    }

    fn spawn_worker(&self, worker_id: &str, health_addr: &str) -> Result<ChildGuard> {
        let child = Command::new(&self.worker_binary)
            .arg("--cluster")
            .arg(CLUSTER)
            .arg("--worker-id")
            .arg(worker_id)
            .arg("--healthz-addr")
            .arg(health_addr)
            .env("HARNX_CONFIG_DIR", self.root.path().join("config"))
            .env("HARNX_DATA_DIR", self.root.path().join("data"))
            .env("HARNX_STATE_DIR", self.root.path().join("state"))
            .env("HARNX_SERVER_SCOPE", self.scope.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn harnx-worker")?;
        Ok(ChildGuard(child))
    }

    async fn session(&self, id: &str, use_tool: bool) -> Result<Arc<NatsSession>> {
        let overrides = SessionOverrides {
            model: Some("mock:test".into()),
            use_tools: use_tool.then(|| vec!["held_wait".into()]),
            ..Default::default()
        };
        Ok(Arc::new(
            NatsSession::new(
                NatsSessionConfig {
                    cluster: CLUSTER.into(),
                    initializer: SessionInitializer::inline("", Default::default(), overrides),
                    session_id: Some(id.into()),
                    activation_route: SessionActivationRoute::ClusterShared,
                },
                self.client.clone(),
                self.jetstream.clone(),
                create_abort_signal(),
            )
            .await?,
        ))
    }
}

fn create_isolated_dirs(root: &Path) -> Result<()> {
    for path in [
        root.join("config/clients"),
        root.join("config/nats_servers"),
        root.join("data"),
        root.join("state"),
    ] {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = socket.read(&mut buffer).await?;
        anyhow::ensure!(read > 0, "model connection closed before request completed");
        request.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                return Ok(request);
            }
        }
    }
}

async fn write_json_response(socket: &mut tokio::net::TcpStream, body: String) -> Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    Ok(())
}

fn tool_call_response() -> String {
    json!({
        "id": "chatcmpl-tool",
        "object": "chat.completion",
        "created": 1,
        "model": "test",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "held-call",
                    "type": "function",
                    "function": {"name": "held_wait", "arguments": "{}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string()
}

fn final_response() -> String {
    json!({
        "id": "chatcmpl-final",
        "object": "chat.completion",
        "created": 1,
        "model": "test",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "resumed after SIGTERM"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    })
    .to_string()
}

fn reserve_address() -> Result<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?.to_string();
    drop(listener);
    Ok(address)
}

async fn wait_for_health(url: &str, expected: reqwest::StatusCode) -> Result<()> {
    let client = reqwest::Client::new();
    tokio::time::timeout(CI_TIMEOUT, async {
        loop {
            if let Ok(response) = client.get(url).send().await {
                if response.status() == expected {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .with_context(|| format!("health endpoint never returned {expected}"))??;
    Ok(())
}

async fn observe_not_ready(url: &str, armed: tokio::sync::oneshot::Sender<()>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut armed = Some(armed);
    tokio::time::timeout(CI_TIMEOUT, async {
        loop {
            match client.get(url).send().await {
                Ok(response) if response.status() == reqwest::StatusCode::OK => {
                    if let Some(armed) = armed.take() {
                        let _ = armed.send(());
                    }
                    // Keep a request in flight so the short shutdown window is observable.
                }
                Ok(response) if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                    return Ok::<_, anyhow::Error>(());
                }
                Ok(_) | Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .context("health endpoint never returned 503 after it was armed")??;
    Ok(())
}

fn send_sigterm(pid: u32) {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(
        result,
        0,
        "send SIGTERM to worker {pid}: {}",
        std::io::Error::last_os_error()
    );
}

async fn wait_for_lease_state(
    jetstream: &async_nats::jetstream::Context,
    session_key: &str,
    present: bool,
) -> Result<()> {
    let config = NatsLeaseConfig::default();
    tokio::time::timeout(CI_TIMEOUT, async {
        loop {
            if let Some(bucket) = open_lease_bucket(jetstream, &config).await {
                let found = lease_holder_in(&bucket, &config, session_key)
                    .await?
                    .is_some();
                if found == present {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

async fn worker_consumer(jetstream: &async_nats::jetstream::Context) -> Result<PullConsumer> {
    let stream = jetstream
        .get_stream(&format!("WORK_NOTIFY_{CLUSTER}"))
        .await?;
    stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

async fn assert_redelivery_after(consumer: &PullConsumer, baseline: usize) -> Result<()> {
    tokio::time::timeout(CI_TIMEOUT, async {
        loop {
            if consumer.get_info().await?.num_redelivered > baseline {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

async fn entries(
    jetstream: &async_nats::jetstream::Context,
    session_key: &str,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    NatsSessionLog::new(jetstream.clone(), session_key)
        .load_events_latest_async()
        .await
}

fn assert_no_cancel(entries: &[(u64, SessionLogEntry)]) {
    assert!(
        !entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })),
        "SIGTERM failover must not append Cancel"
    );
}

struct RunningScenario {
    first: ChildGuard,
    replacement: ChildGuard,
    first_health_url: String,
    session_key: String,
    turn: tokio::task::JoinHandle<Result<NatsTurnResult>>,
}

async fn assert_mid_tool_state(fixture: &Fixture, session_key: &str) -> Result<()> {
    tokio::time::timeout(CI_TIMEOUT, fixture.held_tool.entered.notified()).await?;
    let before = entries(&fixture.jetstream, session_key).await?;
    assert!(before.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::ToolCalls { calls, .. }
            if calls.iter().any(|call| call.id.as_deref() == Some("held-call"))
    )));
    assert!(!before
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. })));
    Ok(())
}

async fn start_scenario(
    fixture: &Fixture,
    name: &str,
    model: &MockModel,
    use_tool: bool,
) -> Result<RunningScenario> {
    fixture.configure_model(&model.api_base)?;
    let first_health = reserve_address()?;
    let first_health_url = format!("http://{first_health}/healthz");
    let first = fixture.spawn_worker(&format!("{name}-first"), &first_health)?;
    wait_for_health(&first_health_url, reqwest::StatusCode::OK).await?;

    let session = fixture.session(name, use_tool).await?;
    let session_key = session.storage_key().to_string();
    let turn = tokio::spawn(async move {
        session
            .run_turn_with_options(
                "run until SIGTERM",
                Arc::new(NullSink),
                None,
                RunTurnOptions {
                    orphan_timeout: Some(FAILOVER_ORPHAN_TIMEOUT),
                    ..Default::default()
                },
            )
            .await
    });
    tokio::time::timeout(CI_TIMEOUT, model.first_started.notified()).await?;
    if use_tool {
        assert_mid_tool_state(fixture, &session_key).await?;
    }
    wait_for_lease_state(&fixture.jetstream, &session_key, true).await?;

    // Start replacement before SIGTERM so runner load can't stretch the
    // intentional lease gap past the client watchdog's grace period.
    let replacement_health = reserve_address()?;
    let replacement_url = format!("http://{replacement_health}/healthz");
    let replacement = fixture.spawn_worker(&format!("{name}-replacement"), &replacement_health)?;
    wait_for_health(&replacement_url, reqwest::StatusCode::OK).await?;
    Ok(RunningScenario {
        first,
        replacement,
        first_health_url,
        session_key,
        turn,
    })
}

async fn signal_and_wait_for_handoff(fixture: &Fixture, scenario: &RunningScenario) -> Result<()> {
    let lease_config = NatsLeaseConfig::default();
    let lease_bucket = open_lease_bucket(&fixture.jetstream, &lease_config)
        .await
        .context("open lease bucket after worker claimed the session")?;
    let mut lease_changes = lease_bucket
        .watch(lease_config.key_for_session(&scenario.session_key))
        .await?;
    let consumer = worker_consumer(&fixture.jetstream).await?;
    let baseline = consumer.get_info().await?.num_redelivered;
    let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();
    let health_url = scenario.first_health_url.clone();
    let not_ready = tokio::spawn(async move { observe_not_ready(&health_url, armed_tx).await });
    armed_rx.await.context("arm readiness observer")?;
    send_sigterm(scenario.first.pid());
    not_ready.await.context("join readiness observer")??;

    tokio::time::timeout(CI_TIMEOUT, async {
        loop {
            let entry = lease_changes
                .next()
                .await
                .context("lease release watch closed")??;
            if entry.operation == Operation::Delete {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await
    .context("worker did not explicitly release its lease after SIGTERM")??;
    assert_redelivery_after(&consumer, baseline).await?;
    wait_for_lease_state(&fixture.jetstream, &scenario.session_key, true)
        .await
        .context("replacement worker never acquired the released session lease")
}

async fn verify_replacement(
    fixture: &Fixture,
    model: &MockModel,
    use_tool: bool,
    mut scenario: RunningScenario,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    model.complete_replacement.notify_one();
    let result = tokio::time::timeout(CI_TIMEOUT, scenario.turn).await???;
    if result.response.is_none() {
        let failure_entries = entries(&fixture.jetstream, &scenario.session_key).await?;
        anyhow::bail!(
            "replacement turn returned no response: error={:?} was_cancelled={} entries={failure_entries:#?}",
            result.error,
            result.was_cancelled
        );
    }
    assert_eq!(result.response.as_deref(), Some("resumed after SIGTERM"));
    let status = scenario.first.wait_for_exit().await?;
    assert!(status.success(), "worker exited unsuccessfully: {status}");
    let after = entries(&fixture.jetstream, &scenario.session_key).await?;
    assert_no_cancel(&after);
    if use_tool {
        assert!(after.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolResults { results, .. }
                if results.iter().any(|result| result.id.as_deref() == Some("held-call"))
        )));
    }
    send_sigterm(scenario.replacement.pid());
    let status = scenario.replacement.wait_for_exit().await?;
    assert!(
        status.success(),
        "replacement worker exited unsuccessfully: {status}"
    );
    Ok(after)
}

async fn run_scenario(
    fixture: &Fixture,
    name: &str,
    model: &MockModel,
    use_tool: bool,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    let scenario = start_scenario(fixture, name, model, use_tool).await?;
    signal_and_wait_for_handoff(fixture, &scenario).await?;
    verify_replacement(fixture, model, use_tool, scenario).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_sigterm_hands_off_mid_llm_and_mid_tool_turns() -> Result<()> {
    let Some(fixture) = Fixture::start().await? else {
        return Ok(());
    };

    let llm = MockModel::start(ModelScript::BlockThenComplete).await?;
    run_scenario(&fixture, "sigterm-mid-llm", &llm, false).await?;

    let tool = MockModel::start(ModelScript::ToolThenComplete).await?;
    run_scenario(&fixture, "sigterm-mid-tool", &tool, true).await?;
    Ok(())
}
