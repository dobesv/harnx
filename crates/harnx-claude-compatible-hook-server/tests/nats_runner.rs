#![cfg(unix)]

use anyhow::{Context, Result};
use harnx_claude_compatible_hook_server::{Args, ClaudeCompatibleHook, CliFailPolicy, HookType};
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResultControl};
use harnx_core::instance::InstanceId;
use harnx_hookset::HookRegistration;
use harnx_hookset_server::{hook_registration_key, serve_over_nats, HOOK_REGISTRY_BUCKET};
use serde_json::json;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "claude-hook-runner-test-token";
const SERVER_NAME: &str = "command-runner";

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
                client.flush().await.context("flush NATS test client")?;
                return Ok(Some(NatsServerHandle {
                    url,
                    _store_dir: store_dir,
                    child,
                }));
            }
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

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &InstanceId,
) -> Result<HookRegistration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await {
            let key = hook_registration_key(instance_id, SERVER_NAME);
            if let Some(value) = store.get(&key).await? {
                return serde_json::from_slice(&value).context("decode hook registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for command runner registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn command_hook_registers_and_answers_over_nats() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let runner = ClaudeCompatibleHook::try_from(Args {
        name: SERVER_NAME.to_string(),
        event: "PreToolUse".to_string(),
        matcher: Some("exec".to_string()),
        priority: 3,
        timeout: Some(5),
        fail_policy: CliFailPolicy::Closed,
        hook_type: HookType::ClaudeCommand,
        command: r#"printf '%s' '{"mutatedToolInput":{"overNats":true}}'"#.to_string(),
        package_dir: None,
    })?;
    let instance_id = InstanceId::new();
    let server_instance_id = instance_id.clone();
    let server_url = server.url.clone();
    let server_task = tokio::spawn(async move {
        serve_over_nats(runner, server_instance_id, &server_url, TOKEN).await
    });
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;

    let registration = wait_for_registration(&client, &instance_id).await?;
    assert_eq!(registration.server, SERVER_NAME);
    assert_eq!(registration.hooks[0].matcher.as_deref(), Some("exec"));

    let payload = HookPayload {
        session_id: "nats-test".to_string(),
        cwd: std::env::current_dir()?,
        resume_count: 0,
        hook_event: HookEvent::PreToolUse {
            tool_name: "exec".to_string(),
            tool_input: json!({"command": "true"}),
            tool_use_id: "tool-use-1".to_string(),
        },
    };
    let message = client
        .request(
            instance_id.hook_subject(SERVER_NAME, "PreToolUse"),
            serde_json::to_vec(&payload)?.into(),
        )
        .await?;
    let outcome: HookOutcome = serde_json::from_slice(&message.payload)?;
    assert_eq!(outcome.control, HookResultControl::Continue);
    assert_eq!(
        outcome.result.mutated_tool_input,
        Some(json!({"overNats": true}))
    );

    server_task.abort();
    Ok(())
}
