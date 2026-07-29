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
    let listener = TcpListener::bind("127.0.0.1:0").context("allocate NATS test port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let store_dir = tempfile::tempdir().context("create NATS test store")?;
    let mut child = Command::new(binary)
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
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(&url)
            .await
        {
            Ok(client) => {
                client.flush().await?;
                break;
            }
            Err(error) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if child.try_wait()?.is_some() {
                    anyhow::bail!("nats-server exited during startup: {error}");
                }
            }
            Err(error) => return Err(error).context("wait for nats-server readiness"),
        }
    }
    Ok(Some(NatsServerHandle {
        url,
        _store_dir: store_dir,
        child,
    }))
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

#[tokio::test(flavor = "multi_thread")]
async fn registers_invokes_caches_and_cancels() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
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

    let registration = wait_for_registration(&client, &instance_id).await?;
    assert_eq!(registration.server, "test");
    assert_eq!(registration.schema_version, TOOL_SCHEMA_VERSION);

    let request = ToolRequest {
        call_id: "call-echo".to_string(),
        tool: "echo".to_string(),
        args: json!({ "value": 42 }),
    };
    let subject = instance_id.tool_subject("test", "echo");
    for _ in 0..2 {
        let message = client
            .request_with_headers(
                subject.clone(),
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
    assert_eq!(toolset.echo_invocations.load(Ordering::SeqCst), 1);

    let slow = ToolRequest {
        call_id: "call-slow".to_string(),
        tool: "slow".to_string(),
        args: json!({}),
    };
    let slow_request = client.request_with_headers(
        instance_id.tool_subject("test", "slow"),
        request_headers(&slow.call_id, "logical-slow"),
        serde_json::to_vec(&slow)?.into(),
    );
    tokio::pin!(slow_request);
    tokio::select! {
        _ = toolset.slow_started.notified() => {}
        result = &mut slow_request => anyhow::bail!("slow request completed before cancellation: {result:?}"),
    }
    let control = ControlMessage {
        call_id: slow.call_id.clone(),
        kind: ControlKind::Cancel,
    };
    client
        .publish_with_headers(
            instance_id.control_subject(),
            request_headers(&slow.call_id, "cancel-slow"),
            serde_json::to_vec(&control)?.into(),
        )
        .await?;
    client.flush().await?;
    let message = tokio::time::timeout(Duration::from_secs(2), &mut slow_request).await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert!(matches!(
        reply.result,
        Err(harnx_toolset::ToolErrorPayload::Fatal(_))
    ));

    server_task.abort();
    let _ = server_task.await;
    Ok(())
}
