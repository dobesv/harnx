//! Isolated `nats-server` process lifecycle for integration tests.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tempfile::TempDir;

use super::TOKEN;

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
fn nats_server_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    which::which("nats-server").ok()
}

pub(crate) async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
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

async fn read_nats_ports_file(dir: &Path, child: &mut Child, deadline: Instant) -> Result<String> {
    loop {
        if let Some(url) = first_nats_client_url(dir) {
            return Ok(url);
        }
        match child.try_wait() {
            Ok(Some(status)) => anyhow::bail!("nats-server exited during startup: {status}"),
            Ok(None) => {}
            Err(error) => return Err(error).context("poll nats-server during startup"),
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for the nats-server ports file in {}",
                dir.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn first_nats_client_url(dir: &Path) -> Option<String> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "ports")
        {
            let contents = std::fs::read_to_string(path).ok()?;
            let ports: serde_json::Value = serde_json::from_str(&contents).ok()?;
            return ports.get("nats")?.get(0)?.as_str().map(str::to_string);
        }
    }
    None
}

async fn wait_for_nats_ready(url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(url)
            .await
        {
            Ok(client) => {
                client.flush().await?;
                return Ok(());
            }
            Err(error) if Instant::now() >= deadline => {
                anyhow::bail!("NATS server at {url} did not become ready: {error}")
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}
