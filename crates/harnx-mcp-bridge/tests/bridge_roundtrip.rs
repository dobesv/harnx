#![cfg(unix)]

use anyhow::{Context, Result};
use harnx_core::instance::ServerScope;
use harnx_mcp_bridge::BridgeToolset;
use harnx_runtime::server_identity::ServerIdentity;
use harnx_toolset::{
    server_identity_token, Registration, ToolReply, ToolRequest, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY,
};
use harnx_toolset_server::{
    registration_key, serve_over_nats, TOOL_REGISTRY_BUCKET, TOOL_SCHEMA_VERSION,
};
use serde_json::json;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "mcp-bridge-roundtrip-token";

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

fn workspace_binary(name: &str, cargo_path: Option<&str>) -> Result<PathBuf> {
    // Cargo's compile-time binary path is preferred. Cross-crate test builds don't always set it,
    // so the fallback resolves the sibling binary next to the test executable's deps directory.
    let (path, mechanism) = if let Some(path) = cargo_path {
        (PathBuf::from(path), "CARGO_BIN_EXE")
    } else {
        let mut path = std::env::current_exe().context("locate current test executable")?;
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        (path.join(name), "test executable sibling")
    };
    anyhow::ensure!(
        path.is_file(),
        "{name} binary missing at {} (resolved via {mechanism})",
        path.display()
    );
    eprintln!("resolved {name} via {mechanism}: {}", path.display());
    Ok(path)
}

fn plans_binary() -> Result<PathBuf> {
    workspace_binary(
        "harnx-plans-tools",
        option_env!("CARGO_BIN_EXE_harnx-plans-tools"),
    )
}

fn mock_mcp_binary() -> Result<PathBuf> {
    workspace_binary(
        "harnx-mock-mcp",
        option_env!("CARGO_BIN_EXE_harnx-mock-mcp"),
    )
}

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    identity_token: &str,
) -> Result<Registration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
            if let Some(value) = store
                .get(registration_key(instance_id, identity_token))
                .await?
            {
                return serde_json::from_slice(&value).context("decode tool registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for tool registration '{identity_token}'");
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
    let instance_id = ServerScope::new();
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
    let identity_token = server_identity_token(None, "", "plans");
    let registration = wait_for_registration(&client, &instance_id, &identity_token).await?;
    assert_eq!(registration.server, "plans");
    assert_eq!(registration.schema_version, TOOL_SCHEMA_VERSION);
    assert_eq!(registration.tools.len(), 15);
    assert!(registration
        .tools
        .iter()
        .any(|tool| tool.name == "list_plans"));
    assert!(registration
        .tools
        .iter()
        .all(|tool| !tool.name.starts_with("plans_")));

    let call_id = "bridge-plans-list";
    let request = ToolRequest {
        call_id: call_id.to_owned(),
        tool: "list_plans".to_owned(),
        args: json!({}),
        parent_session_id: None,
    };
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HDR_CALL_ID, call_id);
    headers.insert(HDR_IDEMPOTENCY_KEY, "bridge-plans-list-idempotency");
    let message = client
        .request_with_headers(
            instance_id.tool_subject(&identity_token, "list_plans"),
            headers,
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert_eq!(reply.call_id, call_id);
    let value = reply.result.expect("list_plans should succeed");
    assert!(value.is_object());
    assert!(value
        .get("content")
        .is_some_and(|content| content.is_array()));

    server_task.abort();
    let _ = server_task.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_registers_raw_search_and_worker_composes_visible_name() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let script_dir = tempfile::tempdir().context("create mock MCP script directory")?;
    let script_path = script_dir.path().join("exa.yaml");
    std::fs::write(
        &script_path,
        "tools:\n  - name: search\n    description: Search documents.\nresponses:\n  - found\n",
    )
    .context("write mock MCP script")?;
    let bridge = BridgeToolset::new(
        "exa",
        vec![
            mock_mcp_binary()?.display().to_string(),
            "--script".to_owned(),
            script_path.display().to_string(),
        ],
    )
    .await?;
    let instance_id = ServerScope::new();
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
    let identity_token = server_identity_token(None, "", "exa");
    let registration = wait_for_registration(&client, &instance_id, &identity_token).await?;

    assert_eq!(registration.package, None);
    assert_eq!(registration.config, "");
    assert_eq!(registration.server, "exa");
    assert_eq!(registration.tools.len(), 1);
    assert_eq!(registration.tools[0].name, "search");
    assert_ne!(registration.tools[0].name, "exa_search");
    assert_eq!(
        ServerIdentity::agent_visible_name(Some("agent-package"), &registration, "search"),
        "exa_search"
    );

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
    let instance_id = ServerScope::new();
    let mut bridge = tokio::process::Command::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"));
    bridge
        .arg("--name")
        .arg("plans")
        .arg("--")
        .arg(plans_binary()?)
        .arg("--mcp-stdio")
        .arg("--dir")
        .arg(plans_dir.path())
        .env("HARNX_SERVER_SCOPE", instance_id.as_str())
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
    let identity_token = server_identity_token(None, "", "plans");
    wait_for_registration(&client, &instance_id, &identity_token).await?;

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

/// A NATS-shaped TCP listener that accepts connections but never sends the
/// server's initial protocol greeting, so `connect()` against it blocks
/// forever without ever producing an error.
///
/// A genuinely unroutable address was considered instead, but its timing is
/// not reliable enough for a test: some sandboxes fail the connection
/// immediately ("network unreachable"), others hang for the OS's SYN-retry
/// timeout (minutes on Linux), and which happens depends on the network
/// namespace the test runs in. A local listener that accepts and stalls
/// reproduces "connect() never resolves" deterministically and fast,
/// regardless of platform.
struct StalledNatsListener {
    url: String,
    // Held so the accepted sockets aren't closed (a close would let the
    // client's connect error out and defeat the point of this listener).
    _connections: Arc<Mutex<Vec<TcpStream>>>,
    _accept_thread: std::thread::JoinHandle<()>,
}

fn spawn_stalled_nats_listener() -> Result<StalledNatsListener> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind stalled NATS listener")?;
    let port = listener.local_addr()?.port();
    let connections = Arc::new(Mutex::new(Vec::new()));
    let accepted = Arc::clone(&connections);
    let accept_thread = std::thread::spawn(move || {
        while let Ok((stream, _addr)) = listener.accept() {
            accepted
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(stream);
        }
    });
    Ok(StalledNatsListener {
        url: format!("nats://127.0.0.1:{port}"),
        _connections: connections,
        _accept_thread: accept_thread,
    })
}

/// Regression test for the initial NATS connect falling out of the
/// child-death race: before the fix, `main` awaited `NatsEndpoint::connect()`
/// as a plain step before the `tokio::select!` against `child_died`, so a
/// child that died while the connect was still stalled left the bridge
/// blocked instead of exiting.
#[tokio::test(flavor = "multi_thread")]
async fn bridge_binary_exits_when_wrapped_child_dies_during_stalled_nats_connect() -> Result<()> {
    let stalled = spawn_stalled_nats_listener()?;
    let plans_dir = tempfile::tempdir().context("create temporary plans directory")?;
    let instance_id = ServerScope::new();
    let mut bridge = tokio::process::Command::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"));
    bridge
        .arg("--name")
        .arg("plans")
        .arg("--")
        .arg(plans_binary()?)
        .arg("--mcp-stdio")
        .arg("--dir")
        .arg(plans_dir.path())
        .env("HARNX_SERVER_SCOPE", instance_id.as_str())
        .env("HARNX_NATS_URL", &stalled.url)
        .env_remove("HARNX_NATS_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut bridge = bridge.spawn().context("spawn harnx-mcp-bridge binary")?;
    let bridge_pid = bridge.id().context("get bridge process ID")?;

    // The wrapped child finishes its MCP handshake (and so shows up under the
    // bridge in /proc) well before the stalled connect could ever resolve on
    // its own; once it's there, the bridge is parked in `connect()`.
    let child_deadline = Instant::now() + Duration::from_secs(10);
    let wrapped_child_pid = loop {
        match direct_child_pid(bridge_pid) {
            Ok(pid) => break pid,
            Err(_) if Instant::now() < child_deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    };
    // SAFETY: wrapped_child_pid was read from the bridge process's direct children.
    anyhow::ensure!(
        unsafe { libc::kill(wrapped_child_pid as libc::pid_t, libc::SIGKILL) } == 0,
        "kill wrapped MCP child {wrapped_child_pid}: {}",
        std::io::Error::last_os_error()
    );

    // A tight deadline, not a fixed sleep, and deliberately shorter than
    // async-nats's own 5s default `connection_timeout`: with the race intact
    // this resolves in well under a second via the child-death branch. With
    // the regression (connect awaited outside the select) the bridge instead
    // blocks until that internal 5s timeout fires on its own, which this
    // deadline is short enough to catch.
    let status = match tokio::time::timeout(Duration::from_secs(2), bridge.wait()).await {
        Ok(status) => status.context("wait for bridge binary")?,
        Err(_) => {
            let _ = bridge.kill().await;
            let _ = bridge.wait().await;
            anyhow::bail!(
                "bridge binary did not exit promptly after wrapped MCP child died during a stalled NATS connect"
            );
        }
    };
    anyhow::ensure!(
        !status.success(),
        "bridge binary exited successfully after wrapped MCP child died during a stalled NATS connect"
    );
    Ok(())
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
