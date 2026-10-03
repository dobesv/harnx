//! Starting a real `nats-server` for a test and waiting until it accepts
//! clients.
use super::TOKEN;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

pub(crate) struct NatsServerHandle {
    pub(crate) url: String,
    _store_dir: TempDir,
    _ports_dir: TempDir,
    _config_dir: Option<TempDir>,
    child: Child,
}

impl Drop for NatsServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    spawn_configured_nats_server(None).await
}

/// [`spawn_nats_server`], with `config` loaded as a nats-server configuration
/// file. The command-line options still win where the two overlap.
pub(crate) async fn spawn_configured_nats_server(
    config: Option<&str>,
) -> Result<Option<NatsServerHandle>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        return Ok(None);
    };

    const MAX_START_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_START_ATTEMPTS {
        let store_dir = tempfile::tempdir().context("create NATS test store")?;
        let ports_dir = tempfile::tempdir().context("create NATS ports dir")?;
        let mut command = Command::new(&binary);
        let config_dir = load_config(&mut command, config)?;
        let mut child = command
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
            .context("spawn nats-server")?;
        let url = read_nats_ports_file(
            ports_dir.path(),
            &mut child,
            Instant::now() + Duration::from_secs(15),
        )
        .await?;

        match wait_for_nats_ready(&mut child, &url).await {
            Ok(()) => {
                return Ok(Some(NatsServerHandle {
                    url,
                    _store_dir: store_dir,
                    _ports_dir: ports_dir,
                    _config_dir: config_dir,
                    child,
                }));
            }
            Err(error) => {
                let exited_during_startup = child.try_wait()?.is_some();
                let _ = child.kill();
                let _ = child.wait();
                if exited_during_startup && attempt < MAX_START_ATTEMPTS {
                    eprintln!(
                        "nats-server exited during startup attempt {attempt}; retrying with a new port: {error:#}"
                    );
                    continue;
                }
                return Err(error).context(format!(
                    "start nats-server after {attempt} attempt{}",
                    if attempt == 1 { "" } else { "s" }
                ));
            }
        }
    }

    unreachable!("NATS startup loop always returns")
}

/// Write `config` to a file and have `command` load it. The returned directory
/// holds the file and has to outlive the server.
fn load_config(command: &mut Command, config: Option<&str>) -> Result<Option<TempDir>> {
    let Some(config) = config else {
        return Ok(None);
    };
    let dir = tempfile::tempdir().context("create NATS config dir")?;
    let path = dir.path().join("nats-server.conf");
    std::fs::write(&path, config).context("write NATS test config")?;
    command.arg("-c").arg(path);
    Ok(Some(dir))
}

async fn wait_for_nats_ready(child: &mut Child, url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let result = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(url)
            .await;
        match result {
            Ok(client) => return client.flush().await.context("flush NATS test client"),
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

fn nats_server_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    which::which("nats-server").ok()
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
