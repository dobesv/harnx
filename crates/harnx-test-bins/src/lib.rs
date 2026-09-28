//! Shared support for workspace integration tests and test-only binaries.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tempfile::TempDir;

/// Isolated JetStream-enabled `nats-server` process for an integration test.
pub struct NatsServerHandle {
    /// Client URL for the server.
    pub url: String,
    _store_dir: TempDir,
    _ports_dir: TempDir,
    child: Child,
}

impl NatsServerHandle {
    /// Client URL for the server.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for NatsServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn an isolated JetStream-enabled `nats-server`.
///
/// Returns `None` when `nats-server` isn't installed so local test runs can
/// skip broker coverage. CI installs the binary and always exercises it.
pub async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
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
    let store_dir = tempfile::tempdir().context("create temporary NATS store directory")?;
    let ports_dir = tempfile::tempdir().context("create temporary NATS ports directory")?;
    let mut child = Command::new(binary)
        .arg("-js")
        .arg("-sd")
        .arg(store_dir.path())
        .arg("-a")
        .arg("127.0.0.1")
        .arg("-p")
        .arg("-1")
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

fn nats_server_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    which::which("nats-server").ok()
}

async fn wait_for_nats_ready(url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match tokio::time::timeout(Duration::from_secs(1), async_nats::connect(url)).await {
            Ok(Ok(client)) => {
                client.flush().await.context("flush NATS test connection")?;
                return Ok(());
            }
            Ok(Err(error)) if Instant::now() >= deadline => {
                return Err(error).context("NATS test server did not become ready");
            }
            Err(_) if Instant::now() >= deadline => {
                anyhow::bail!("NATS test server did not become ready: connection timed out");
            }
            Ok(Err(_)) | Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn read_nats_ports_file(
    directory: &Path,
    child: &mut Child,
    deadline: Instant,
) -> Result<String> {
    loop {
        if let Some(url) = first_nats_client_url(directory) {
            return Ok(url);
        }
        match child.try_wait() {
            Ok(Some(status)) => anyhow::bail!("nats-server exited during startup: {status}"),
            Ok(None) => {}
            Err(error) => return Err(error).context("poll nats-server during startup"),
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for nats-server ports file in {}",
                directory.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn first_nats_client_url(directory: &Path) -> Option<String> {
    for entry in std::fs::read_dir(directory).ok()? {
        let path = entry.ok()?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "ports")
        {
            let contents = std::fs::read_to_string(path).ok()?;
            let ports: serde_json::Value = serde_json::from_str(&contents).ok()?;
            return Some(ports.get("nats")?.get(0)?.as_str()?.to_string());
        }
    }
    None
}
