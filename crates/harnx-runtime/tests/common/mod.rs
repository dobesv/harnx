use anyhow::{Context, Result};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::time::sleep;

pub struct NatsServerHandle {
    pub url: String,
    _store_dir: TempDir,
    child: Child,
}

#[derive(Debug, Clone, Default)]
pub struct SpawnNatsServerOptions {
    pub auth_token: Option<String>,
}

impl NatsServerHandle {
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

pub async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    spawn_nats_server_with_options(SpawnNatsServerOptions::default()).await
}

pub async fn spawn_nats_server_with_options(
    options: SpawnNatsServerOptions,
) -> Result<Option<NatsServerHandle>> {
    let binary = match nats_server_binary() {
        Some(binary) => binary,
        None => {
            eprintln!("skipping NATS integration test: nats-server binary not found");
            return Ok(None);
        }
    };

    // Allocating a free port and then handing it to nats-server has an
    // inherent TOCTOU race: under parallel test execution another server can
    // grab the same port between our probe and nats-server's bind. Retry the
    // whole spawn-and-wait with a fresh port a few times to make the harness
    // robust instead of intermittently failing (e.g. lease_contention).
    let mut last_err = None;
    for _ in 0..5 {
        match try_spawn_nats_once(&binary, &options).await {
            Ok(handle) => return Ok(Some(handle)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("failed to spawn nats-server")))
}

async fn try_spawn_nats_once(
    binary: &std::path::Path,
    options: &SpawnNatsServerOptions,
) -> Result<NatsServerHandle> {
    let port = free_tcp_port()?;
    let store_dir = tempfile::tempdir().context("Failed to create temp NATS store dir")?;
    let mut command = Command::new(binary);
    command
        .arg("-js")
        .arg("-sd")
        .arg(store_dir.path())
        .arg("-p")
        .arg(port.to_string());
    if let Some(token) = &options.auth_token {
        command.arg("--auth").arg(token);
    }
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to spawn {}", binary.display()))?;

    let url = format!("nats://127.0.0.1:{port}");
    if let Err(e) = wait_for_nats_ready(&url, options.auth_token.as_deref()).await {
        // Readiness failed (likely the port was stolen by a parallel server);
        // reap this child so the retry starts clean.
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }

    Ok(NatsServerHandle {
        url,
        _store_dir: store_dir,
        child,
    })
}

#[allow(dead_code)]
pub fn harnx_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HARNX_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    let mut path = std::env::current_exe().ok()?;
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) { "harnx.exe" } else { "harnx" });
    path.is_file().then_some(path)
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

fn free_tcp_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("Failed to allocate free TCP port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

async fn wait_for_nats_ready(url: &str, auth_token: Option<&str>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let connect_result = match auth_token {
            Some(token) => {
                tokio::time::timeout(
                    Duration::from_secs(1),
                    async_nats::ConnectOptions::new()
                        .token(token.to_string())
                        .connect(url),
                )
                .await
            }
            None => tokio::time::timeout(Duration::from_secs(1), async_nats::connect(url)).await,
        };
        match connect_result {
            Ok(Ok(client)) => {
                client
                    .flush()
                    .await
                    .context("Failed to flush NATS connection")?;
                return Ok(());
            }
            Ok(Err(error)) if Instant::now() < deadline => {
                eprintln!("waiting for NATS server at {url} (attempt {attempts}): {error}");
                sleep(Duration::from_millis(100)).await;
            }
            Err(_) if Instant::now() < deadline => {
                eprintln!(
                    "waiting for NATS server at {url} (attempt {attempts}): connection attempt timed out"
                );
                sleep(Duration::from_millis(100)).await;
            }
            Ok(Err(error)) => {
                return Err(error).with_context(|| {
                    format!(
                        "NATS server at {url} did not become ready within 15s after {attempts} attempts"
                    )
                });
            }
            Err(_) => {
                anyhow::bail!(
                    "NATS server at {url} did not become ready within 15s after {attempts} attempts: connection attempts timed out"
                );
            }
        }
    }
}
