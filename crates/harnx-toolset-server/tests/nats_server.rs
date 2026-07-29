use anyhow::{Context, Result};
use async_trait::async_trait;
use harnx_core::instance::InstanceId;
use harnx_toolset::{
    ControlKind, ControlMessage, Registration, ToolInvokeError, ToolReply, ToolRequest, ToolSpec,
    Toolset, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY,
};
use harnx_toolset_server::{
    registration_key, serve_over_nats, TOOL_REGISTRY_BUCKET, TOOL_SCHEMA_VERSION,
};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "toolset-test-token";

struct NatsServerHandle {
    url: String,
    _store_dir: TempDir,
    child: Child,
}

impl Drop for NatsServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        return Ok(None);
    };

    const MAX_START_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_START_ATTEMPTS {
        let listener = TcpListener::bind("127.0.0.1:0").context("allocate NATS test port")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let store_dir = tempfile::tempdir().context("create NATS test store")?;
        let mut child = Command::new(&binary)
            .arg("-js")
            .arg("-sd")
            .arg(store_dir.path())
            .arg("-p")
            .arg(port.to_string())
            .arg("--auth")
            .arg(TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn nats-server")?;
        let url = format!("nats://127.0.0.1:{port}");

        match wait_for_nats_ready(&mut child, &url).await {
            Ok(()) => {
                return Ok(Some(NatsServerHandle {
                    url,
                    _store_dir: store_dir,
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
struct TestToolset {
    echo_invocations: Arc<AtomicUsize>,
    slow_started: Arc<Notify>,
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

fn request_headers(call_id: &str, idempotency_key: &str) -> async_nats::HeaderMap {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HDR_CALL_ID, call_id);
    headers.insert(HDR_IDEMPOTENCY_KEY, idempotency_key);
    headers
}

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &InstanceId,
) -> Result<Registration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
            let key = registration_key(instance_id, "test");
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

struct TestHarness {
    _server: NatsServerHandle,
    server_task: tokio::task::JoinHandle<Result<()>>,
    client: async_nats::Client,
    instance_id: InstanceId,
    toolset: TestToolset,
}

impl TestHarness {
    async fn start() -> Result<Option<Self>> {
        let Some(server) = spawn_nats_server().await? else {
            return Ok(None);
        };
        let instance_id = InstanceId::new();
        let toolset = TestToolset::default();
        let server_url = server.url.clone();
        let server_toolset = toolset.clone();
        let server_instance_id = instance_id.clone();
        let server_task = tokio::spawn(async move {
            serve_over_nats(server_toolset, server_instance_id, &server_url, TOKEN).await
        });
        let client = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(&server.url)
            .await?;
        Ok(Some(Self {
            _server: server,
            server_task,
            client,
            instance_id,
            toolset,
        }))
    }

    fn echo_subject(&self) -> String {
        self.instance_id.tool_subject("test", "echo")
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

async fn assert_registration(harness: &TestHarness) -> Result<()> {
    let registration = wait_for_registration(&harness.client, &harness.instance_id).await?;
    assert_eq!(registration.server, "test");
    assert_eq!(registration.schema_version, TOOL_SCHEMA_VERSION);
    Ok(())
}

async fn assert_idempotent_replay(harness: &TestHarness) -> Result<()> {
    let request = ToolRequest {
        call_id: "call-echo".to_string(),
        tool: "echo".to_string(),
        args: json!({ "value": 42 }),
    };
    for _ in 0..2 {
        let message = harness
            .client
            .request_with_headers(
                harness.echo_subject(),
                request_headers(&request.call_id, "logical-echo"),
                serde_json::to_vec(&request)?.into(),
            )
            .await?;
        let reply: ToolReply = serde_json::from_slice(&message.payload)?;
        assert_eq!(
            reply.result.expect("echo request should succeed"),
            json!({ "value": 42 })
        );
    }
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn assert_concurrent_idempotency(harness: &TestHarness) -> Result<()> {
    let args = json!({ "value": 43, "delay_ms": 100 });
    let first = ToolRequest {
        call_id: "call-concurrent-a".to_string(),
        tool: "echo".to_string(),
        args: args.clone(),
    };
    let second = ToolRequest {
        call_id: "call-concurrent-b".to_string(),
        tool: "echo".to_string(),
        args: args.clone(),
    };
    let first_call = harness.client.request_with_headers(
        harness.echo_subject(),
        request_headers(&first.call_id, "logical-concurrent"),
        serde_json::to_vec(&first)?.into(),
    );
    let second_call = harness.client.request_with_headers(
        harness.echo_subject(),
        request_headers(&second.call_id, "logical-concurrent"),
        serde_json::to_vec(&second)?.into(),
    );
    let (first_reply, second_reply) = tokio::join!(first_call, second_call);
    for (message, expected_id) in [
        (first_reply?, first.call_id.as_str()),
        (second_reply?, second.call_id.as_str()),
    ] {
        let reply: ToolReply = serde_json::from_slice(&message.payload)?;
        assert_eq!(reply.call_id, expected_id);
        assert_eq!(reply.result.expect("concurrent echo should succeed"), args);
    }
    assert_eq!(
        harness.toolset.echo_invocations.load(Ordering::SeqCst),
        2,
        "concurrent duplicate must execute once"
    );
    Ok(())
}

async fn request_early_failure(
    harness: &TestHarness,
    headers: async_nats::HeaderMap,
    payload: Vec<u8>,
    expected: (&str, &str),
) -> Result<()> {
    let request = async_nats::Request::new()
        .headers(headers)
        .payload(payload.into())
        .timeout(Some(Duration::from_secs(1)));
    let message = tokio::time::timeout(
        Duration::from_secs(1),
        harness.client.send_request(harness.echo_subject(), request),
    )
    .await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert_eq!(reply.call_id, expected.0);
    match reply.result {
        Err(harnx_toolset::ToolErrorPayload::Recoverable(message)) => {
            assert!(message.contains(expected.1), "{message}");
        }
        other => anyhow::bail!("expected recoverable early failure, got {other:?}"),
    }
    Ok(())
}

async fn assert_early_failure_replies(harness: &TestHarness) -> Result<()> {
    request_early_failure(
        harness,
        request_headers("call-malformed", "logical-malformed"),
        b"not-json".to_vec(),
        ("call-malformed", "decode tool request payload"),
    )
    .await?;
    let mismatched = ToolRequest {
        call_id: "payload-call".to_string(),
        tool: "echo".to_string(),
        args: json!({}),
    };
    request_early_failure(
        harness,
        request_headers("header-call", "logical-mismatch"),
        serde_json::to_vec(&mismatched)?,
        ("header-call", "call ID header does not match payload"),
    )
    .await?;
    let missing_key = ToolRequest {
        call_id: "call-missing-key".to_string(),
        tool: "echo".to_string(),
        args: json!({}),
    };
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HDR_CALL_ID, missing_key.call_id.as_str());
    request_early_failure(
        harness,
        headers,
        serde_json::to_vec(&missing_key)?,
        ("call-missing-key", "missing Idempotency-Key"),
    )
    .await
}

async fn assert_cancellation(harness: &TestHarness) -> Result<()> {
    let request = ToolRequest {
        call_id: "call-slow".to_string(),
        tool: "slow".to_string(),
        args: json!({}),
    };
    let slow_request = harness.client.request_with_headers(
        harness.instance_id.tool_subject("test", "slow"),
        request_headers(&request.call_id, "logical-slow"),
        serde_json::to_vec(&request)?.into(),
    );
    tokio::pin!(slow_request);
    tokio::select! {
        _ = harness.toolset.slow_started.notified() => {}
        result = &mut slow_request => anyhow::bail!("slow request completed before cancellation: {result:?}"),
    }
    let control = ControlMessage {
        call_id: request.call_id.clone(),
        kind: ControlKind::Cancel,
    };
    harness
        .client
        .publish_with_headers(
            harness.instance_id.control_subject(),
            request_headers(&request.call_id, "cancel-slow"),
            serde_json::to_vec(&control)?.into(),
        )
        .await?;
    harness.client.flush().await?;
    let message = tokio::time::timeout(Duration::from_secs(2), &mut slow_request).await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert!(matches!(
        reply.result,
        Err(harnx_toolset::ToolErrorPayload::Fatal(_))
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn registers_invokes_caches_and_cancels() -> Result<()> {
    let Some(harness) = TestHarness::start().await? else {
        return Ok(());
    };
    assert_registration(&harness).await?;
    assert_idempotent_replay(&harness).await?;
    assert_concurrent_idempotency(&harness).await?;
    assert_early_failure_replies(&harness).await?;
    assert_cancellation(&harness).await
}
