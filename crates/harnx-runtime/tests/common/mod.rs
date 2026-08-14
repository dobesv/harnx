use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::time::sleep;

pub struct NatsServerHandle {
    pub url: String,
    _store_dir: TempDir,
    _ports_dir: TempDir,
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
    let store_dir = tempfile::tempdir().context("Failed to create temp NATS store dir")?;
    let ports_dir = tempfile::tempdir().context("Failed to create temp NATS ports dir")?;
    let mut command = Command::new(binary);
    command
        .arg("-js")
        .arg("-sd")
        .arg(store_dir.path())
        .arg("-a")
        .arg("127.0.0.1")
        .arg("-p")
        .arg("-1")
        .arg("--ports_file_dir")
        .arg(ports_dir.path());
    if let Some(token) = &options.auth_token {
        command.arg("--auth").arg(token);
    }
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to spawn {}", binary.display()))?;

    let url = match read_nats_ports_file(
        ports_dir.path(),
        &mut child,
        Instant::now() + Duration::from_secs(15),
    )
    .await
    {
        Ok(url) => url,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    if let Err(e) = wait_for_nats_ready(&url, options.auth_token.as_deref()).await {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }

    Ok(NatsServerHandle {
        url,
        _store_dir: store_dir,
        _ports_dir: ports_dir,
        child,
    })
}

#[allow(dead_code)]
pub fn harnx_worker_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HARNX_WORKER_BIN") {
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
    path.push(if cfg!(windows) {
        "harnx-worker.exe"
    } else {
        "harnx-worker"
    });
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
