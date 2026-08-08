#![cfg(unix)]

use anyhow::{Context, Result};
use harnx_claude_compatible_hook_server::{Args, ClaudeCompatibleHook, CliFailPolicy};
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResultControl};
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_hookset::{HookRegistration, HARNX_HOOK_NAME};
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

struct HookServerHandle(Child);

impl Drop for HookServerHandle {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
    server_name: &str,
) -> Result<HookRegistration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await {
            let key = hook_registration_key(instance_id, server_name);
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
        persistent: false,
        command: ["printf", "%s", r#"{"mutatedToolInput":{"overNats":true}}"#]
            .iter()
            .map(|word| word.to_string())
            .collect(),
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

    let registration = wait_for_registration(&client, &instance_id, SERVER_NAME).await?;
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

#[tokio::test(flavor = "multi_thread")]
async fn command_hook_uses_env_name_with_end_of_options_separator() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let assigned_name = "hook-env-name-000";
    let instance_id = InstanceId::new();
    let child = Command::new(env!("CARGO_BIN_EXE_harnx-claude-compatible-hook-server"))
        .args([
            "--event",
            "PreToolUse",
            "--matcher",
            "exec",
            "--",
            "printf",
            "{}",
        ])
        .env(HARNX_HOOK_NAME, assigned_name)
        .env(HARNX_INSTANCE_ID, instance_id.as_str())
        .env("HARNX_NATS_URL", &server.url)
        .env("HARNX_NATS_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn command hook with environment-assigned name")?;
    let _hook_server = HookServerHandle(child);
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;

    let registration = wait_for_registration(&client, &instance_id, assigned_name).await?;
    assert_eq!(registration.server, assigned_name);
    assert_eq!(registration.hooks[0].matcher.as_deref(), Some("exec"));
    Ok(())
}

/// Poll the hook registration key until it matches `present`, or fail at
/// `deadline`. No blind sleeps: the assertion is only as slow as the actual
/// state change.
async fn wait_for_key_presence(
    client: &async_nats::Client,
    key: &str,
    present: bool,
    deadline: Instant,
) -> Result<()> {
    let jetstream = async_nats::jetstream::new(client.clone());
    loop {
        if let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await {
            let found = store.get(key).await?.is_some();
            if found == present {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for hook registration '{key}' to become present={present}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Proves that a SIGTERM sent directly to a running
/// `harnx-claude-compatible-hook-server` process (as Kubernetes would send
/// to a pod) triggers deregistration, not just that the plumbing between a
/// `CancellationToken` and the serve loop is wired correctly.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_removes_the_hook_registration() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let assigned_name = "hook-sigterm-000";
    let instance_id = InstanceId::new();
    let child = Command::new(env!("CARGO_BIN_EXE_harnx-claude-compatible-hook-server"))
        .args([
            "--event",
            "PreToolUse",
            "--matcher",
            "exec",
            "--",
            "printf",
            "{}",
        ])
        .env(HARNX_HOOK_NAME, assigned_name)
        .env(HARNX_INSTANCE_ID, instance_id.as_str())
        .env("HARNX_NATS_URL", &server.url)
        .env("HARNX_NATS_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn command hook for SIGTERM test")?;
    let pid = child.id();
    let mut hook_server = HookServerHandle(child);

    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let key = hook_registration_key(&instance_id, assigned_name);

    wait_for_key_presence(
        &client,
        &key,
        true,
        Instant::now() + Duration::from_secs(10),
    )
    .await?;

    let killed = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(
        killed, 0,
        "failed to send SIGTERM to harnx-claude-compatible-hook-server"
    );

    wait_for_key_presence(
        &client,
        &key,
        false,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .context("hook registration should be gone soon after SIGTERM, not left to expire")?;

    let _ = hook_server.0.wait();
    Ok(())
}
