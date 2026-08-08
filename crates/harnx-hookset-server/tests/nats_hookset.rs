use anyhow::{Context, Result};
use async_trait::async_trait;
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_core::instance::ServerScope;
use harnx_hookset::{
    FailPolicy, Hook, HookRegistration, HookSpec, HOOK_PROTOCOL_VERSION, HOOK_SCHEMA_VERSION,
};
use harnx_hookset_server::{hook_registration_key, serve_over_nats, HOOK_REGISTRY_BUCKET};
use serde_json::json;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "hookset-server-test-token";

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

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<HookRegistration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await {
            let key = hook_registration_key(instance_id, "echo");
            if let Some(value) = store.get(&key).await? {
                return serde_json::from_slice(&value).context("decode hook registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for echo hook registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct EchoHook;

#[async_trait]
impl Hook for EchoHook {
    fn name(&self) -> &str {
        "echo"
    }

    fn hooks(&self) -> Vec<HookSpec> {
        vec![HookSpec {
            event: "PreToolUse".to_string(),
            matcher: None,
            priority: 0,
            timeout_secs: None,
            fail_policy: FailPolicy::Closed,
        }]
    }

    async fn handle_hook(&self, _payload: HookPayload) -> HookOutcome {
        HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult {
                mutated_tool_input: Some(json!({"seen": true})),
                ..Default::default()
            },
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hookset_registers_and_serves_hook_over_nats() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let instance_id = ServerScope::new();
    let server_instance_id = instance_id.clone();
    let server_url = server.url.clone();
    let server_task = tokio::spawn(async move {
        serve_over_nats(EchoHook, server_instance_id, &server_url, TOKEN).await
    });
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;

    let registration = match wait_for_registration(&client, &instance_id).await {
        Ok(registration) => registration,
        Err(error) if server_task.is_finished() => {
            return Err(anyhow::anyhow!(
                "hook server exited before registration ({error:#}): {:?}",
                server_task.await
            ));
        }
        Err(error) => return Err(error),
    };
    assert_eq!(registration.server, "echo");
    assert_eq!(registration.hooks.len(), 1);
    assert_eq!(registration.hooks[0].event, "PreToolUse");
    assert_eq!(registration.schema_version, HOOK_SCHEMA_VERSION);
    assert_eq!(registration.proto_version, HOOK_PROTOCOL_VERSION);

    let payload = HookPayload {
        session_id: "test-session".to_string(),
        cwd: std::env::current_dir()?,
        resume_count: 0,
        hook_event: HookEvent::PreToolUse {
            tool_name: "example".to_string(),
            tool_input: json!({"input": true}),
            tool_use_id: "tool-use-1".to_string(),
        },
    };
    let message = client
        .request(
            instance_id.hook_subject("echo", "PreToolUse"),
            serde_json::to_vec(&payload)?.into(),
        )
        .await?;
    let outcome: HookOutcome =
        serde_json::from_slice(&message.payload).context("decode hook outcome")?;
    assert_eq!(outcome.control, HookResultControl::Continue);
    assert_eq!(
        outcome.result.mutated_tool_input,
        Some(json!({"seen": true}))
    );

    server_task.abort();
    drop(server);
    Ok(())
}
