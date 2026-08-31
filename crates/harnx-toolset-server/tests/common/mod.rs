//! Shared nats-server/harness plumbing for `harnx-toolset-server` integration
//! tests: spawning a real `nats-server`, a stub `Toolset`, and a running
//! `serve_with_shutdown` instance to exercise. Split out of `nats_server.rs`
//! because this infrastructure shares no data with the test bodies
//! themselves, and keeping it there pushed that file's cohesion (LCOM4) over
//! CodeScene's threshold.

use anyhow::{Context, Result};
use async_trait::async_trait;
use harnx_core::instance::ServerScope;
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY};
use harnx_toolset_server::{serve_with_shutdown, ServeLifecycle};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(crate) const TOKEN: &str = "toolset-test-token";

pub(crate) struct NatsServerHandle {
    pub(crate) url: String,
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

pub(crate) async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        return Ok(None);
    };

    const MAX_START_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_START_ATTEMPTS {
        let store_dir = tempfile::tempdir().context("create NATS test store")?;
        let ports_dir = tempfile::tempdir().context("create NATS ports dir")?;
        let mut child = Command::new(&binary)
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
            .context("spawn nats-server")?;
        let url = read_nats_ports_file(
            ports_dir.path(),
            &mut child,
            Instant::now() + Duration::from_secs(15),
        )
        .await?;

        match wait_for_nats_ready(&mut child, &url).await {
            Ok(()) => {
                return Ok(Some(NatsServerHandle {
                    url,
                    _store_dir: store_dir,
                    _ports_dir: ports_dir,
                    child,
                }));
            }
            Err(error) => {
                let exited_during_startup = child.try_wait()?.is_some();
                let _ = child.kill();
                let _ = child.wait();
                if exited_during_startup && attempt < MAX_START_ATTEMPTS {
                    eprintln!(
                        "nats-server exited during startup attempt {attempt}; retrying with a new port: {error:#}"
                    );
                    continue;
                }
                return Err(error).context(format!(
                    "start nats-server after {attempt} attempt{}",
                    if attempt == 1 { "" } else { "s" }
                ));
            }
        }
    }

    unreachable!("NATS startup loop always returns")
}

async fn wait_for_nats_ready(child: &mut Child, url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let result = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(url)
            .await;
        match result {
            Ok(client) => return client.flush().await.context("flush NATS test client"),
            Err(error) if child.try_wait()?.is_some() => {
                anyhow::bail!("nats-server exited during startup: {error}");
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error).context("wait for nats-server readiness"),
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

#[derive(Clone, Default)]
pub(crate) struct TestToolset {
    pub(crate) echo_invocations: Arc<AtomicUsize>,
    pub(crate) slow_started: Arc<Notify>,
}

#[async_trait]
impl Toolset for TestToolset {
    fn name(&self) -> &str {
        "test"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo".to_string(),
            description: "echo input".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        match tool {
            "echo" => {
                self.echo_invocations.fetch_add(1, Ordering::SeqCst);
                if let Some(delay_ms) = args.get("delay_ms").and_then(Value::as_u64) {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                Ok(args)
            }
            "slow" => {
                self.slow_started.notify_one();
                cancel.cancelled().await;
                Err(ToolInvokeError::Fatal("cancelled".to_string()))
            }
            _ => Err(ToolInvokeError::Recoverable("unknown tool".to_string())),
        }
    }
}

pub(crate) fn request_headers(call_id: &str, idempotency_key: &str) -> async_nats::HeaderMap {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HDR_CALL_ID, call_id);
    headers.insert(HDR_IDEMPOTENCY_KEY, idempotency_key);
    headers
}

pub(crate) async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<harnx_toolset::Registration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream
            .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
            .await
        {
            let key = harnx_toolset_server::registration_key(instance_id, "____test");
            if let Some(value) = store.get(&key).await? {
                return serde_json::from_slice(&value).context("decode registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for tool registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(crate) struct TestHarness {
    // `pub(crate)` (not just field-private) because tests connect a second,
    // independent server against the same nats-server URL.
    pub(crate) _server: NatsServerHandle,
    // `Option` so `shutdown()` can take this out without consuming the whole
    // harness — dropping `_server` would kill nats-server out from under the
    // rest of the test, which still needs it to inspect KV state afterward.
    pub(crate) server_task: Option<tokio::task::JoinHandle<Result<()>>>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) client: async_nats::Client,
    pub(crate) instance_id: ServerScope,
    pub(crate) toolset: TestToolset,
    pub(crate) readiness: harnx_healthz::Readiness,
}

impl TestHarness {
    pub(crate) async fn start() -> Result<Option<Self>> {
        let Some(server) = spawn_nats_server().await? else {
            return Ok(None);
        };
        let instance_id = ServerScope::new();
        let toolset = TestToolset::default();
        let shutdown = CancellationToken::new();
        let readiness = harnx_healthz::Readiness::default();
        let server_client = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(&server.url)
            .await?;
        let server_toolset = toolset.clone();
        let server_instance_id = instance_id.clone();
        let server_shutdown = shutdown.clone();
        let server_readiness = readiness.clone();
        let server_task = tokio::spawn(async move {
            serve_with_shutdown(
                Arc::new(server_toolset),
                server_instance_id,
                NatsConnection {
                    client: server_client,
                    replicas: 1,
                },
                ServeLifecycle::new(server_shutdown, Some(server_readiness)),
            )
            .await
        });
        let client = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(&server.url)
            .await?;
        Ok(Some(Self {
            _server: server,
            server_task: Some(server_task),
            shutdown,
            client,
            instance_id,
            toolset,
            readiness,
        }))
    }

    pub(crate) fn echo_subject(&self) -> String {
        self.instance_id.tool_subject("____test", "echo")
    }

    /// Trigger a graceful shutdown and wait for `serve_with_shutdown` to
    /// unwind and run its exit cleanup (deleting the KV registration), while
    /// leaving the rest of the harness (including the still-running
    /// nats-server) intact so the test can keep inspecting KV state
    /// afterward.
    pub(crate) async fn shutdown(&mut self) {
        self.shutdown.cancel();
        if let Some(server_task) = self.server_task.take() {
            let _ = server_task.await;
        }
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        if let Some(server_task) = self.server_task.take() {
            server_task.abort();
        }
    }
}

/// Read the client URL out of the ports file nats-server writes once it has
/// bound its listeners.
///
/// The file is named `<executable_name>_<pid>.ports`, so it's found by scanning
/// the (private) directory rather than by rebuilding the name — `NATS_SERVER_BIN`
/// can point at a differently named binary. nats-server writes into the file
/// directly rather than renaming it into place, so a partial read is possible;
/// failing to parse is treated the same as not-yet-written.
async fn read_nats_ports_file(
    dir: &std::path::Path,
    child: &mut Child,
    deadline: Instant,
) -> Result<String> {
    loop {
        if let Some(url) = first_nats_client_url(dir) {
            return Ok(url);
        }
        // `std::process::Child` does not kill on drop, so every failure path here
        // has to reap the server or it keeps running after the test gives up.
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!("nats-server exited during startup: {status}")
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("poll nats-server during startup");
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "timed out waiting for the nats-server ports file in {}",
                dir.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn first_nats_client_url(dir: &std::path::Path) -> Option<String> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.extension().is_some_and(|ext| ext == "ports") {
            let contents = std::fs::read_to_string(&path).ok()?;
            let parsed: serde_json::Value = serde_json::from_str(&contents).ok()?;
            let url = parsed.get("nats")?.get(0)?.as_str()?;
            return Some(url.to_string());
        }
    }
    None
}
