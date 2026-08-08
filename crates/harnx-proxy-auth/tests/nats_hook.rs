#![cfg(unix)]

use anyhow::{Context, Result};
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResultControl};
use harnx_core::instance::ServerScope;
use harnx_hookset::{FailPolicy, HookRegistration};
use harnx_hookset_server::{hook_registration_key, serve_over_nats, HOOK_REGISTRY_BUCKET};
use harnx_proxy_auth::hook::ProxyAuthHook;
use serde_json::{json, Map};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "proxy-auth-hook-test-token";
const ASSIGNED_NAME: &str = "hook-a1b2c3d4-000";

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
    instance_id: &ServerScope,
) -> Result<HookRegistration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await {
            let key = hook_registration_key(instance_id, ASSIGNED_NAME);
            if let Some(value) = store.get(&key).await? {
                return serde_json::from_slice(&value).context("decode hook registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for proxy-auth registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_auth_registers_and_mutates_tool_env_over_nats() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let proxy_port = 4312;
    let ca_cert_path = PathBuf::from("/tmp/proxy-auth-nats-test-ca.pem");
    let extra_env = Map::from_iter([("ACLI_CONFIG_DIR".to_string(), json!("/tmp/acli-config"))]);
    let hook = ProxyAuthHook::new(
        ASSIGNED_NAME.to_string(),
        proxy_port,
        ca_cert_path.clone(),
        extra_env,
    );
    let instance_id = ServerScope::new();
    let server_instance_id = instance_id.clone();
    let server_url = server.url.clone();
    let server_task =
        tokio::spawn(
            async move { serve_over_nats(hook, server_instance_id, &server_url, TOKEN).await },
        );
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;

    let registration = wait_for_registration(&client, &instance_id).await?;
    assert_eq!(registration.server, ASSIGNED_NAME);
    assert_eq!(registration.hooks.len(), 1);
    assert_eq!(registration.hooks[0].event, "PreToolUse");
    assert_eq!(registration.hooks[0].matcher.as_deref(), Some("exec|spawn"));
    assert_eq!(registration.hooks[0].fail_policy, FailPolicy::Closed);

    let payload = HookPayload {
        session_id: "proxy-auth-nats-test".to_string(),
        cwd: std::env::current_dir()?,
        resume_count: 0,
        hook_event: HookEvent::PreToolUse {
            tool_name: "exec".to_string(),
            tool_input: json!({"command": "true", "env": {"EXISTING": "kept"}}),
            tool_use_id: "tool-use-1".to_string(),
        },
    };
    let message = client
        .request(
            instance_id.hook_subject(ASSIGNED_NAME, "PreToolUse"),
            serde_json::to_vec(&payload)?.into(),
        )
        .await?;
    let outcome: HookOutcome = serde_json::from_slice(&message.payload)?;
    assert_eq!(outcome.control, HookResultControl::Continue);
    let mutated = outcome
        .result
        .mutated_tool_input
        .context("proxy-auth must return mutated tool input")?;
    let env = mutated["env"].as_object().context("mutated env object")?;
    assert_eq!(
        env["HTTP_PROXY"],
        json!(format!("http://127.0.0.1:{proxy_port}"))
    );
    assert_eq!(
        env["HTTPS_PROXY"],
        json!(format!("http://127.0.0.1:{proxy_port}"))
    );
    assert_eq!(env["SSL_CERT_FILE"], json!(ca_cert_path));
    assert_eq!(env["NODE_EXTRA_CA_CERTS"], json!(ca_cert_path));
    assert_eq!(env["ACLI_CONFIG_DIR"], json!("/tmp/acli-config"));
    assert_eq!(env["EXISTING"], json!("kept"));

    server_task.abort();
    Ok(())
}
