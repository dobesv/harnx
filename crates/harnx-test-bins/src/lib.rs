//! Shared support for workspace integration tests and test-only binaries.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tempfile::TempDir;

/// Isolated JetStream-enabled `nats-server` process for an integration test.
///
/// Dropping the handle kills the server process, so a fixture that hands out
/// only the URL must also keep the handle (or leak it) for as long as anything
/// talks to that broker.
pub struct NatsServerHandle {
    /// Client URL for the server.
    pub url: String,
    store_dir: Option<TempDir>,
    _ports_dir: TempDir,
    child: Child,
}

impl NatsServerHandle {
    /// Client URL for the server.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Kill the server. Call [`Self::wait`] to reap it before continuing a test.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }

    /// Wait for the server to exit, normally after calling [`Self::kill`].
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    /// Transfer the store directory guard to the caller, at most once.
    ///
    /// Dropping it while the server runs deletes JetStream's store from under
    /// it, so callers that need the server afterwards should hold it longer.
    pub fn take_store_dir(&mut self) -> Option<TempDir> {
        self.store_dir.take()
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
    spawn_configured_nats_server("").await
}

/// Spawn an isolated broker with extra settings in a nats-server config file.
/// The normal test server flags supply JetStream, a private store and a free port.
// qual:test_helper
pub async fn spawn_configured_nats_server(config: &str) -> Result<Option<NatsServerHandle>> {
    spawn_nats_server_with_port(config, None).await
}

/// Isolated JetStream-enabled `nats-server` on one specific port, for tests
/// that must bring a broker up later at an address a client already holds:
/// unlike [`spawn_nats_server`], the port is bound exactly as requested and
/// spawning fails if it is taken.
// qual:test_helper
pub async fn spawn_nats_server_on_port(port: u16) -> Result<Option<NatsServerHandle>> {
    spawn_nats_server_with_port("", Some(port)).await
}

async fn spawn_nats_server_with_port(
    config: &str,
    port: Option<u16>,
) -> Result<Option<NatsServerHandle>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        return Ok(None);
    };

    const MAX_SPAWN_ATTEMPTS: usize = 5;
    let mut result = try_spawn_nats_server(&binary, config, port).await;
    for _ in 1..MAX_SPAWN_ATTEMPTS {
        if result.is_ok() {
            break;
        }
        result = try_spawn_nats_server(&binary, config, port).await;
    }
    result.map(Some)
}

async fn try_spawn_nats_server(
    binary: &Path,
    config: &str,
    port: Option<u16>,
) -> Result<NatsServerHandle> {
    let store_dir = tempfile::tempdir().context("create temporary NATS store directory")?;
    let ports_dir = tempfile::tempdir().context("create temporary NATS ports directory")?;
    let mut command = Command::new(binary);
    if !config.is_empty() {
        let path = ports_dir.path().join("nats.conf");
        std::fs::write(&path, config).context("write test NATS configuration")?;
        command.arg("-c").arg(path);
    }
    let mut child = command
        .arg("-js")
        .arg("-sd")
        .arg(store_dir.path())
        .arg("-a")
        .arg("127.0.0.1")
        .arg("-p")
        .arg(port.map_or_else(|| "-1".to_string(), |port| port.to_string()))
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
        store_dir: Some(store_dir),
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
        let probe = async {
            let client = async_nats::connect(url)
                .await
                .context("connect to NATS test server")?;
            client.flush().await.context("flush NATS test connection")
        };
        match tokio::time::timeout(Duration::from_secs(1), probe).await {
            Ok(Ok(())) => return Ok(()),
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
