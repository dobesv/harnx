//! End-to-end NATS tests for prompt streaming, cancellation, and idle tracking.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock as AcpContentBlock, InitializeRequest, NewSessionRequest,
    PromptRequest, PromptResponse, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent,
};
use anyhow::{Context, Result};
use futures::StreamExt;
use harnx_core::agent_config::AgentConfig;
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_runtime::config::{Config, GlobalConfig, NatsServerConfig};
use harnx_runtime::nats_worker::{run_worker_daemon, worker_ready_subject, WorkerDaemonConfig};
use harnx_runtime::{AgentCallFn, SessionActivationRoute, SessionInitializer};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const TOKEN: &str = "acp-test-token";
const CLUSTER: &str = "acp-test";
const AGENT_NAME: &str = "acp-test-agent";
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

struct NatsServerHandle {
    url: String,
    _store_dir: TempDir,
    _ports_dir: TempDir,
    child: Child,
}

impl Drop for NatsServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct BackgroundTasks(Vec<tokio::task::JoinHandle<()>>);

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

struct AbortObserver {
    observed: Arc<AtomicBool>,
    abort: harnx_core::abort::AbortSignal,
}

impl Drop for AbortObserver {
    fn drop(&mut self) {
        if self.abort.aborted() {
            self.observed.store(true, Ordering::SeqCst);
        }
    }
}

fn nats_server_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    which::which("nats-server").ok()
}

async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        return Ok(None);
    };

    let mut last_error = None;
    for _ in 0..5 {
        match try_spawn_nats_server(&binary).await {
            Ok(server) => return Ok(Some(server)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to spawn nats-server")))
}

async fn try_spawn_nats_server(binary: &Path) -> Result<NatsServerHandle> {
    let store_dir = tempfile::tempdir().context("create NATS test store")?;
    let ports_dir = tempfile::tempdir().context("create NATS ports dir")?;
    let mut child = Command::new(binary)
        .arg("-js")
        .arg("-sd")
        .arg(store_dir.path())
        .arg("-a")
        .arg("127.0.0.1")
        .arg("-p")
        .arg("-1")
        .arg("--auth")
        .arg(TOKEN)
        .arg("--ports_file_dir")
        .arg(ports_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {}", binary.display()))?;

    let url = match read_nats_ports_file(
        ports_dir.path(),
        &mut child,
        Instant::now() + Duration::from_secs(15),
    )
    .await
    {
        Ok(url) => url,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };

    if let Err(error) = wait_for_nats_ready(&url).await {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    Ok(NatsServerHandle {
        url,
        _store_dir: store_dir,
        _ports_dir: ports_dir,
        child,
    })
}

async fn read_nats_ports_file(dir: &Path, child: &mut Child, deadline: Instant) -> Result<String> {
    loop {
        if let Some(url) = first_nats_client_url(dir) {
            return Ok(url);
        }
        match child.try_wait() {
            Ok(Some(status)) => anyhow::bail!("nats-server exited during startup: {status}"),
            Ok(None) => {}
            Err(error) => return Err(error).context("poll nats-server during startup"),
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for the nats-server ports file in {}",
                dir.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn first_nats_client_url(dir: &Path) -> Option<String> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "ports")
        {
            let contents = std::fs::read_to_string(path).ok()?;
            let ports: serde_json::Value = serde_json::from_str(&contents).ok()?;
            return ports.get("nats")?.get(0)?.as_str().map(str::to_string);
        }
    }
    None
}

async fn wait_for_nats_ready(url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(url)
            .await
        {
            Ok(client) => {
                client.flush().await?;
                return Ok(());
            }
            Err(error) if Instant::now() >= deadline => {
                anyhow::bail!("NATS server at {url} did not become ready: {error}")
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

fn test_config(url: &str) -> GlobalConfig {
    let mut agent = AgentConfig::from_markdown(
        AGENT_NAME,
        "---\nmodel: test:test-model\n---\nACP integration test agent",
    )
    .expect("parse test agent");
    agent.set_resolved_model(harnx_core::model::Model::new("test", "test-model"));

    Arc::new(parking_lot::RwLock::new(Config {
        data: harnx_core::config_data::ConfigData {
            model_id: "test:test-model".to_string(),
            dry_run: false,
            ..Default::default()
        },
        agent: Some(harnx_runtime::config::Agent::new(agent)),
        model: harnx_core::model::Model::new("test", "test-model"),
        nats_servers: vec![NatsServerConfig {
            name: CLUSTER.to_string(),
            url: url.to_string(),
            token: Some(TOKEN.to_string()),
            replicas: None,
            tls: Some(false),
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            ignore_discovered_servers: None,
            agents: vec![],
        }],
        ..Default::default()
    }))
}

async fn spawn_worker(
    config: GlobalConfig,
    call_fn: AgentCallFn,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let client = {
        let config = config.read().clone();
        config.nats_client(CLUSTER).await?
    };
    let mut readiness = client.subscribe(worker_ready_subject(CLUSTER)).await?;
    client.flush().await?;

    let mut daemon = tokio::spawn(run_worker_daemon(
        config,
        WorkerDaemonConfig::managing(CLUSTER, "acp-integration-worker"),
        Some(call_fn),
        None,
    ));
    tokio::select! {
        ready = tokio::time::timeout(TEST_TIMEOUT, readiness.next()) => {
            ready.context("worker did not announce readiness")?
                .context("worker readiness subscription closed")?;
        }
        stopped = &mut daemon => anyhow::bail!("worker stopped before readiness: {stopped:?}"),
    }
    Ok(daemon)
}

fn new_agent(config: &GlobalConfig) -> Arc<harnx_acp_server::HarnxAgent> {
    let nats_config = harnx_acp_server::NatsAgentConfig {
        runtime_config: Arc::clone(config),
        cluster: CLUSTER.to_string(),
        activation_route: SessionActivationRoute::ClusterShared,
        session_initializer: SessionInitializer::inline(
            "ACP integration test agent",
            Default::default(),
            Default::default(),
        ),
    };
    Arc::new(harnx_acp_server::HarnxAgent::with_nats_config(
        AGENT_NAME.to_string(),
        nats_config,
    ))
}

async fn initialize_and_create_session(agent: &harnx_acp_server::HarnxAgent) -> Result<SessionId> {
    let initialized = agent
        .initialize(InitializeRequest::new(acp::schema::ProtocolVersion::V1))
        .await?;
    assert_eq!(
        initialized.protocol_version,
        acp::schema::ProtocolVersion::V1
    );

    let session = agent
        .new_session(NewSessionRequest::new(std::env::current_dir()?))
        .await?;
    Ok(session.session_id)
}

async fn attach_notification_client(
    agent: Arc<harnx_acp_server::HarnxAgent>,
) -> Result<(
    mpsc::UnboundedReceiver<SessionNotification>,
    BackgroundTasks,
)> {
    let (agent_stream, client_stream) = tokio::io::duplex(64 * 1024);
    let (agent_read, agent_write) = tokio::io::split(agent_stream);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let agent_transport = acp::ByteStreams::new(agent_write.compat_write(), agent_read.compat());
    let client_transport = acp::ByteStreams::new(client_write.compat_write(), client_read.compat());

    let (connection_tx, connection_rx) = oneshot::channel();
    let agent_task = tokio::spawn(async move {
        let _ = acp::Agent
            .builder()
            .connect_with(agent_transport, async move |connection| {
                let _ = connection_tx.send(connection);
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;
    });

    let (notification_tx, notification_rx) = mpsc::unbounded_channel();
    let client_task = tokio::spawn(async move {
        let _ = acp::Client
            .builder()
            .on_receive_notification_from(
                acp::Agent,
                async move |notification: SessionNotification, _connection| {
                    let _ = notification_tx.send(notification);
                    Ok(())
                },
                acp::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |_connection| {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;
    });

    let connection = tokio::time::timeout(TEST_TIMEOUT, connection_rx)
        .await
        .context("ACP notification transport did not connect")?
        .context("ACP agent transport closed during setup")?;
    agent.set_connection(connection).await;
    Ok((
        notification_rx,
        BackgroundTasks(vec![agent_task, client_task]),
    ))
}

fn text_prompt(session_id: SessionId, text: &str) -> PromptRequest {
    PromptRequest::new(
        session_id,
        vec![AcpContentBlock::Text(TextContent::new(text.to_string()))],
    )
}

fn notification_text(notification: SessionNotification) -> Option<String> {
    match notification.update {
        SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            AcpContentBlock::Text(text) => Some(text.text),
            _ => None,
        },
        _ => None,
    }
}

fn gated_call_fn(started: Arc<Semaphore>, release: Arc<Semaphore>) -> AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        Box::pin(async move {
            started.add_permits(1);
            release
                .acquire()
                .await
                .expect("release semaphore closed")
                .forget();
            Ok((
                "done".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

fn spawn_prompt(
    agent: Arc<harnx_acp_server::HarnxAgent>,
    session_id: SessionId,
) -> tokio::task::JoinHandle<acp::Result<PromptResponse>> {
    tokio::spawn(async move {
        agent
            .prompt(text_prompt(session_id, "touch lifecycle"))
            .await
    })
}

async fn wait_for_model_start(
    started: &Semaphore,
    prompt: &mut tokio::task::JoinHandle<acp::Result<PromptResponse>>,
) -> Result<()> {
    tokio::select! {
        permit = started.acquire() => permit.context("started semaphore closed")?.forget(),
        result = prompt => anyhow::bail!("prompt ended before model call started: {result:?}"),
        _ = tokio::time::sleep(TEST_TIMEOUT) => anyhow::bail!("worker model call did not start"),
    }
    Ok(())
}

async fn session_touch(
    agent: &harnx_acp_server::HarnxAgent,
    session_id: &str,
    missing_context: &'static str,
) -> Result<Duration> {
    agent
        .session_last_touched(session_id)
        .await
        .context(missing_context)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_turn_streams_in_order() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move {
            for text in ["hello ", "from ", "assistant"] {
                harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text(text.to_string())],
                }));
                tokio::task::yield_now().await;
            }
            Ok((
                "hello from assistant".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    });
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let (mut notifications, _transport) = attach_notification_client(Arc::clone(&agent)).await?;
    let session_id = initialize_and_create_session(&agent).await?;

    let response = tokio::time::timeout(
        TEST_TIMEOUT,
        agent.prompt(text_prompt(session_id.clone(), "say hello")),
    )
    .await
    .context("prompt did not finish")??;
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let mut chunks = Vec::new();
    while chunks.len() < 3 {
        let notification = tokio::time::timeout(TEST_TIMEOUT, notifications.recv())
            .await
            .context("timed out waiting for session/update")?
            .context("ACP notification stream closed")?;
        assert_eq!(notification.session_id, session_id);
        if let Some(text) = notification_text(notification) {
            chunks.push(text);
        }
    }
    assert_eq!(chunks, ["hello ", "from ", "assistant"]);

    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_prompt_on_same_session_is_rejected() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let call_fn: AgentCallFn = {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        Arc::new(move |_input, _config, _abort| {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            Box::pin(async move {
                started.add_permits(1);
                release
                    .acquire()
                    .await
                    .expect("release semaphore closed")
                    .forget();
                Ok((
                    "first turn complete".to_string(),
                    None,
                    vec![],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            })
        })
    };
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let first_prompt = tokio::spawn({
        let agent = Arc::clone(&agent);
        let session_id = session_id.clone();
        async move { agent.prompt(text_prompt(session_id, "first")).await }
    });
    tokio::time::timeout(TEST_TIMEOUT, started.acquire())
        .await
        .context("first model call did not start")??
        .forget();

    let overlap_error = agent
        .prompt(text_prompt(session_id, "overlap"))
        .await
        .expect_err("overlapping turn must be rejected");
    assert!(
        overlap_error
            .to_string()
            .contains("already has an in-flight turn"),
        "unexpected overlap error: {overlap_error}"
    );

    release.add_permits(1);
    let response = tokio::time::timeout(TEST_TIMEOUT, first_prompt)
        .await
        .context("first prompt did not complete")???;
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_model_error_is_returned_to_acp_caller() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move { anyhow::bail!("simulated model failure") })
    });
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let error = tokio::time::timeout(
        TEST_TIMEOUT,
        agent.prompt(text_prompt(session_id, "fail this turn")),
    )
    .await
    .context("failed model turn did not return")?
    .expect_err("worker model failure must be an ACP error");
    assert!(
        error.to_string().contains("simulated model failure"),
        "unexpected ACP error: {error}"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_turn_cancel_stops_turn() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let started = Arc::new(Semaphore::new(0));
    let worker_observed_cancel = Arc::new(AtomicBool::new(false));
    let call_fn: AgentCallFn = {
        let started = Arc::clone(&started);
        let worker_observed_cancel = Arc::clone(&worker_observed_cancel);
        Arc::new(move |_input, _config, abort| {
            let started = Arc::clone(&started);
            let worker_observed_cancel = Arc::clone(&worker_observed_cancel);
            Box::pin(async move {
                let _guard = AbortObserver {
                    observed: Arc::clone(&worker_observed_cancel),
                    abort: abort.clone(),
                };
                started.add_permits(1);
                harnx_core::abort::wait_abort_signal(&abort).await;
                worker_observed_cancel.store(true, Ordering::SeqCst);
                anyhow::bail!("cancelled test model call")
            })
        })
    };
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let mut prompt = tokio::spawn({
        let agent = Arc::clone(&agent);
        let session_id = session_id.clone();
        async move { agent.prompt(text_prompt(session_id, "wait")).await }
    });
    tokio::select! {
        permit = started.acquire() => permit.context("started semaphore closed")?.forget(),
        result = &mut prompt => anyhow::bail!("prompt ended before model call started: {result:?}"),
        _ = tokio::time::sleep(TEST_TIMEOUT) => anyhow::bail!("worker model call did not start"),
    }

    agent
        .cancel(CancelNotification::new(session_id))
        .await
        .context("cancel prompt")?;
    let response = tokio::time::timeout(TEST_TIMEOUT, prompt)
        .await
        .context("cancelled prompt did not return")???;
    assert_eq!(response.stop_reason, StopReason::Cancelled);
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !worker_observed_cancel.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("worker did not observe durable cancellation")?;

    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn touch_updates_on_prompt_lifecycle() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let worker = spawn_worker(
        Arc::clone(&config),
        gated_call_fn(Arc::clone(&started), Arc::clone(&release)),
    )
    .await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;
    let session_key = session_id.0.to_string();
    let created_touch =
        session_touch(&agent, &session_key, "missing created session touch").await?;

    tokio::time::sleep(Duration::from_millis(5)).await;
    let mut prompt = spawn_prompt(Arc::clone(&agent), session_id.clone());
    wait_for_model_start(&started, &mut prompt).await?;
    let started_touch = session_touch(&agent, &session_key, "missing prompt start touch").await?;
    assert!(started_touch > created_touch);

    tokio::time::sleep(Duration::from_millis(5)).await;
    release.add_permits(1);
    let response = tokio::time::timeout(TEST_TIMEOUT, prompt)
        .await
        .context("prompt did not complete")???;
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let completed_touch =
        session_touch(&agent, &session_key, "missing prompt completion touch").await?;
    assert!(completed_touch > started_touch);

    tokio::time::sleep(Duration::from_millis(5)).await;
    agent
        .cancel(CancelNotification::new(session_id))
        .await
        .context("cancel idle session")?;
    let cancelled_touch = session_touch(&agent, &session_key, "missing cancel touch").await?;
    assert!(cancelled_touch > completed_touch);

    worker.abort();
    let _ = worker.await;
    Ok(())
}
