#![cfg(unix)]

use anyhow::{Context, Result};
use harnx_core::instance::InstanceId;
use harnx_mcp_bridge::BridgeToolset;
use harnx_toolset::{Registration, ToolReply, ToolRequest, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY};
use harnx_toolset_server::{
    registration_key, serve_over_nats, TOOL_REGISTRY_BUCKET, TOOL_SCHEMA_VERSION,
};
use serde_json::json;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "mcp-bridge-roundtrip-token";

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
        .with_context(|| format!("spawn {}", binary.display()))?;
    let url = format!("nats://127.0.0.1:{port}");

    if let Err(error) = wait_for_nats_ready(&url).await {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(NatsServerHandle {
        url,
        _store_dir: store_dir,
        child,
    })
}

async fn wait_for_nats_ready(url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let connection = tokio::time::timeout(
            Duration::from_secs(1),
            async_nats::ConnectOptions::new()
                .token(TOKEN.to_string())
                .connect(url),
        )
        .await;
        match connection {
            Ok(Ok(client)) => return client.flush().await.context("flush NATS test client"),
            Ok(Err(_)) | Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(Err(error)) => return Err(error).context("wait for nats-server readiness"),
            Err(_) => anyhow::bail!("timed out waiting for nats-server readiness"),
        }
    }
}

fn plans_binary() -> Result<PathBuf> {
    // Cargo's compile-time binary path is preferred. Cross-crate test builds don't always set it,
    // so the fallback resolves the sibling binary next to the test executable's deps directory.
    let (path, mechanism) = if let Some(path) = option_env!("CARGO_BIN_EXE_harnx-mcp-plans") {
        (PathBuf::from(path), "CARGO_BIN_EXE_harnx-mcp-plans")
    } else {
        let mut path = std::env::current_exe().context("locate current test executable")?;
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        (path.join("harnx-mcp-plans"), "test executable sibling")
    };
    anyhow::ensure!(
        path.is_file(),
        "harnx-mcp-plans binary missing at {} (resolved via {mechanism})",
        path.display()
    );
    eprintln!(
        "resolved harnx-mcp-plans via {mechanism}: {}",
        path.display()
    );
    Ok(path)
}

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &InstanceId,
) -> Result<Registration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
            if let Some(value) = store.get(registration_key(instance_id, "plans")).await? {
                return serde_json::from_slice(&value).context("decode plans registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for plans tool registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_registers_plans_and_round_trips_an_invoke() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let plans_dir = tempfile::tempdir().context("create temporary plans directory")?;
    let bridge = BridgeToolset::new(
        "plans",
        vec![
            plans_binary()?.display().to_string(),
            "--mcp-stdio".to_owned(),
            "--dir".to_owned(),
            plans_dir.path().display().to_string(),
        ],
    )
    .await?;
    let instance_id = InstanceId::new();
    let server_instance_id = instance_id.clone();
    let nats_url = server.url.clone();
    let server_task =
        tokio::spawn(
            async move { serve_over_nats(bridge, server_instance_id, &nats_url, TOKEN).await },
        );

    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let registration = wait_for_registration(&client, &instance_id).await?;
    assert_eq!(registration.server, "plans");
    assert_eq!(registration.schema_version, TOOL_SCHEMA_VERSION);
    assert_eq!(registration.tools.len(), 15);
    assert!(registration
        .tools
        .iter()
        .all(|tool| tool.name.starts_with("plans_")));

    let call_id = "bridge-plans-list";
    let request = ToolRequest {
        call_id: call_id.to_owned(),
        tool: "plans_list_plans".to_owned(),
        args: json!({}),
        parent_session_id: None,
    };
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HDR_CALL_ID, call_id);
    headers.insert(HDR_IDEMPOTENCY_KEY, "bridge-plans-list-idempotency");
    let message = client
        .request_with_headers(
            instance_id.tool_subject("plans", "plans_list_plans"),
            headers,
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert_eq!(reply.call_id, call_id);
    let value = reply.result.expect("plans_list_plans should succeed");
    assert!(value.is_object());
    assert!(value
        .get("content")
        .is_some_and(|content| content.is_array()));

    server_task.abort();
    let _ = server_task.await;
    Ok(())
}

#[cfg(target_os = "linux")]
fn direct_child_pid(parent_pid: u32) -> Result<u32> {
    // /proc avoids depending on pgrep in minimal Linux test environments.
    for entry in std::fs::read_dir("/proc").context("scan /proc for bridge child")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let status = match std::fs::read_to_string(entry.path().join("status")) {
            Ok(status) => status,
            Err(_) => continue,
        };
        let is_child = status.lines().any(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse::<u32>().ok())
                == Some(parent_pid)
        });
        if is_child {
            return Ok(pid);
        }
    }
    anyhow::bail!("no direct child found for bridge process {parent_pid}")
}

#[cfg(not(target_os = "linux"))]
fn direct_child_pid(parent_pid: u32) -> Result<u32> {
    // pgrep -P is available on the BSD and macOS Unix hosts covered by this test.
    let output = Command::new("pgrep")
        .arg("-P")
        .arg(parent_pid.to_string())
        .output()
        .context("run pgrep to find bridge child")?;
    anyhow::ensure!(
        output.status.success(),
        "pgrep found no direct child for bridge process {parent_pid}"
    );
    let stdout = String::from_utf8(output.stdout).context("decode pgrep output")?;
    stdout
        .lines()
        .next()
        .context("pgrep returned empty output")?
        .trim()
        .parse()
        .context("parse bridge child process ID")
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_binary_exits_when_wrapped_child_dies() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let plans_dir = tempfile::tempdir().context("create temporary plans directory")?;
    let instance_id = InstanceId::new();
    let mut bridge = tokio::process::Command::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"));
    bridge
        .arg("--name")
        .arg("plans")
        .arg("--")
        .arg(plans_binary()?)
        .arg("--mcp-stdio")
        .arg("--dir")
        .arg(plans_dir.path())
        .env("HARNX_INSTANCE_ID", instance_id.as_str())
        .env("HARNX_NATS_URL", &server.url)
        .env("HARNX_NATS_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut bridge = bridge.spawn().context("spawn harnx-mcp-bridge binary")?;
    let bridge_pid = bridge.id().context("get bridge process ID")?;

    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;
    wait_for_registration(&client, &instance_id).await?;

    let wrapped_child_pid = direct_child_pid(bridge_pid)?;
    // SAFETY: wrapped_child_pid was read from the bridge process's direct children.
    anyhow::ensure!(
        unsafe { libc::kill(wrapped_child_pid as libc::pid_t, libc::SIGKILL) } == 0,
        "kill wrapped MCP child {wrapped_child_pid}: {}",
        std::io::Error::last_os_error()
    );

    let status = match tokio::time::timeout(Duration::from_secs(10), bridge.wait()).await {
        Ok(status) => status.context("wait for bridge binary")?,
        Err(_) => {
            let _ = bridge.kill().await;
            let _ = bridge.wait().await;
            anyhow::bail!("bridge binary did not exit after wrapped MCP child died");
        }
    };
    anyhow::ensure!(
        !status.success(),
        "bridge binary exited successfully after wrapped MCP child died"
    );
    Ok(())
}
