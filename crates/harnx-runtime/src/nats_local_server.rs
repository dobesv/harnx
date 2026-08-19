//! Lifecycle management for the single shared, per-user local NATS server.
//!
//! Ownership is represented by an exclusive file lock. The process holding the
//! lock owns the child server; other processes discover authenticated connection
//! details through an atomically replaced metadata file.

use anyhow::{bail, Context, Result};
use harnx_core::config_paths::{
    nats_runtime_dir, nats_runtime_lock_file, nats_runtime_ports_file, nats_runtime_store_dir,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use tokio::time::sleep;
use uuid::Uuid;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const RETRY_DELAY: Duration = Duration::from_millis(100);
const SPAWN_ATTEMPTS: usize = 5;
/// How long to wait for nats-server to write its ports file. Generous: it is
/// written as soon as the listeners are bound, well before JetStream is ready.
const PORT_REPORT_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(unix)]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Authenticated connection details for the shared local NATS server.
///
/// Keep this value alive if this caller started the server. Dropping the owner
/// value stops and reaps its child; values returned to joiners do not control
/// server lifetime.
pub struct SharedNatsServer {
    pub url: String,
    pub token: String,
    pub nonce: String,
    owner: Option<ServerOwner>,
}

impl SharedNatsServer {
    /// Whether this handle owns the server process and lifetime lock.
    pub fn is_owner(&self) -> bool {
        self.owner.is_some()
    }

    /// Process ID when this handle owns the server; joiners return `None`.
    pub fn server_process_id(&self) -> Option<u32> {
        self.owner.as_ref().map(|owner| owner.child.id())
    }

    /// Whether this handle still identifies the active shared broker.
    ///
    /// Joiners cannot keep the owner process alive, so their cached discovery
    /// result is valid only while the same nonce is published and another
    /// process still holds the lifetime lock. Checking those local files avoids
    /// a new NATS connection for every config lookup while still detecting an
    /// owner that exited and left stale metadata behind.
    pub async fn is_current(&mut self) -> bool {
        if let Some(owner) = self.owner.as_mut() {
            return matches!(owner.child.try_wait(), Ok(None));
        }

        let cached_identity = (self.url.clone(), self.token.clone(), self.nonce.clone());
        tokio::task::spawn_blocking(move || joiner_is_current(cached_identity))
            .await
            .unwrap_or(false)
    }
}

fn joiner_is_current(cached_identity: (String, String, String)) -> bool {
    let Ok(metadata) = read_metadata() else {
        return false;
    };
    let discovered_identity = (metadata.url(), metadata.token, metadata.nonce);
    if discovered_identity != cached_identity {
        return false;
    }

    let Ok(lock_file) = open_lock_file() else {
        return false;
    };
    match crate::file_lock::try_lock_exclusive(&lock_file) {
        Ok(true) => {
            let _ = lock_file.unlock();
            false
        }
        Ok(false) => true,
        Err(_) => false,
    }
}

impl std::fmt::Debug for SharedNatsServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedNatsServer")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("nonce", &self.nonce)
            .field("is_owner", &self.is_owner())
            .finish()
    }
}

struct ServerOwner {
    child: Child,
    lock_file: File,
    nonce: String,
    config_path: PathBuf,
}

impl Drop for ServerOwner {
    fn drop(&mut self) {
        // Keep lock held until child has exited and stale discovery metadata has
        // been removed. A waiter can only become owner after cleanup completes.
        stop_server(&mut self.child);
        remove_metadata_if_nonce_matches(&self.nonce);
        remove_file_if_present(&self.config_path, "shared local NATS config");
        let _ = self.lock_file.unlock();
    }
}

fn stop_server(child: &mut Child) {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        if result == 0 {
            let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ServerMetadata {
    port: u16,
    nonce: String,
    token: String,
}

impl ServerMetadata {
    fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.port)
    }
}

/// Start or join the single shared per-user `nats-server`.
///
/// Owners hold `<data_dir>/nats/v1/nats.lock` for their lifetime. Joiners wait
/// for atomically published metadata, authenticate, then re-read the metadata
/// to prove the nonce did not change during connection.
pub async fn ensure_shared_server() -> Result<SharedNatsServer> {
    prepare_runtime_directories()?;
    let lock_file = open_lock_file()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;

    loop {
        if crate::file_lock::try_lock_exclusive(&lock_file)
            .context("failed to acquire shared local NATS lock")?
        {
            return start_owned_server(lock_file).await;
        }

        if let Some(server) = try_join_server().await {
            return Ok(server);
        }

        if Instant::now() >= deadline {
            bail!(
                "shared local NATS owner did not publish valid ports.json within {}s",
                STARTUP_TIMEOUT.as_secs()
            );
        }
        sleep(RETRY_DELAY).await;
    }
}

fn prepare_runtime_directories() -> Result<()> {
    let runtime_dir = nats_runtime_dir();
    fs::create_dir_all(&runtime_dir).with_context(|| {
        format!(
            "failed to create NATS runtime directory {}",
            runtime_dir.display()
        )
    })?;
    fs::create_dir_all(nats_runtime_store_dir()).with_context(|| {
        format!(
            "failed to create NATS store directory {}",
            nats_runtime_store_dir().display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "failed to secure NATS runtime directory {}",
                    runtime_dir.display()
                )
            },
        )?;
    }
    Ok(())
}

fn open_lock_file() -> Result<File> {
    let path = nats_runtime_lock_file();
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&path)
        .with_context(|| format!("failed to open shared local NATS lock {}", path.display()))
}

async fn start_owned_server(lock_file: File) -> Result<SharedNatsServer> {
    let binary = nats_server_binary()?;
    verify_nats_server_version(&binary)?;
    let nonce = Uuid::new_v4().to_string();
    let token = Uuid::new_v4().simple().to_string();
    let config_path = nats_runtime_dir().join("nats.conf");
    let mut last_error = None;

    for _ in 0..SPAWN_ATTEMPTS {
        match spawn_once(&binary, &config_path, &token).await {
            Ok((child, port)) => {
                let metadata = ServerMetadata {
                    port,
                    nonce: nonce.clone(),
                    token: token.clone(),
                };
                if let Err(error) = write_metadata_atomically(&metadata) {
                    let mut child = child;
                    let _ = child.kill();
                    let _ = child.wait();
                    remove_file_if_present(&config_path, "shared local NATS config");
                    return Err(error);
                }
                return Ok(SharedNatsServer {
                    url: metadata.url(),
                    token,
                    nonce: nonce.clone(),
                    owner: Some(ServerOwner {
                        child,
                        lock_file,
                        nonce,
                        config_path,
                    }),
                });
            }
            Err(error) => last_error = Some(error),
        }
    }

    remove_file_if_present(&config_path, "shared local NATS config");
    Err(last_error.expect("at least one NATS spawn attempt must run")).context(format!(
        "failed to start shared local NATS server after {SPAWN_ATTEMPTS} attempts"
    ))
}

async fn spawn_once(binary: &Path, config_path: &Path, token: &str) -> Result<(Child, u16)> {
    write_server_config_atomically(config_path, token)?;

    let mut command = Command::new(binary);
    command
        .arg("-c")
        .arg(config_path)
        .stdin(Stdio::null())
        // Where our own logs go. Discarding this left a broker that refuses to
        // start — a bad config, a port it can't bind — saying nothing at all.
        .stdout(harnx_core::logging::child_output_sink())
        .stderr(harnx_core::logging::child_output_sink());
    configure_parent_death(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", binary.display()))?;

    let child_pid = child.id();
    let ports_path = port_report_path(binary, child_pid);
    let port = match read_bound_port(&mut child, &ports_path).await {
        Ok(port) => port,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let url = format!("nats://127.0.0.1:{port}");
    if let Err(error) = wait_for_nats_ready(&url, token, &mut child).await {
        let _ = child.kill();
        let _ = child.wait();
        remove_ports_file(ports_path);
        return Err(error);
    }
    // nats-server removes its own ports file on a clean exit, but this server is
    // usually killed, so drop it now that the port is known and republished in
    // harnx's ports.json.
    remove_ports_file(ports_path);
    Ok((child, port))
}

/// Wait for nats-server to report the port it bound.
///
/// nats-server writes into the file rather than renaming it into place, so a
/// parse failure is treated the same as not-yet-written.
async fn read_bound_port(child: &mut Child, path: &Path) -> Result<u16> {
    let deadline = Instant::now() + PORT_REPORT_TIMEOUT;
    loop {
        if let Some(port) = parse_reported_port(path) {
            return Ok(port);
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("shared local NATS exited during startup: {status}");
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "shared local NATS did not report its port within {}s",
                PORT_REPORT_TIMEOUT.as_secs()
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn port_report_path(binary: &Path, pid: u32) -> PathBuf {
    let executable_name = binary
        .file_name()
        .unwrap_or_else(|| OsStr::new("nats-server"));
    let mut report_name = executable_name.to_os_string();
    report_name.push(format!("_{pid}.ports"));
    nats_runtime_dir().join(report_name)
}

fn parse_reported_port(path: &Path) -> Option<u16> {
    let contents = fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let url = parsed.get("nats")?.get(0)?.as_str()?;
    url.rsplit_once(':')?.1.parse().ok()
}

fn remove_ports_file(path: PathBuf) {
    remove_file_if_present(&path, "shared local NATS ports report");
}

#[cfg(target_os = "linux")]
fn configure_parent_death(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    let parent_pid = std::process::id() as libc::pid_t;
    // SAFETY: pre_exec only invokes async-signal-safe libc calls and captures a
    // plain pid. The parent check closes the fork-to-prctl race.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                libc::raise(libc::SIGTERM);
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_parent_death(_command: &mut Command) {}

async fn try_join_server() -> Option<SharedNatsServer> {
    let before = read_metadata().ok()?;
    let url = before.url();
    let client = tokio::time::timeout(
        CONNECT_TIMEOUT,
        async_nats::ConnectOptions::new()
            .token(before.token.clone())
            .connect(&url),
    )
    .await
    .ok()?
    .ok()?;
    // The owner can exit after the TCP/auth handshake but before this flush.
    // async-nats then waits for a reconnect to the now-dead ephemeral port, so
    // bound the validation just like the connect itself.
    if !matches!(
        tokio::time::timeout(CONNECT_TIMEOUT, client.flush()).await,
        Ok(Ok(()))
    ) {
        return None;
    }

    // A successful connection is only accepted if discovery data remained the
    // same throughout it. This rejects stale metadata while a new owner starts.
    let after = read_metadata().ok()?;
    if before != after || before.nonce.is_empty() {
        return None;
    }

    Some(SharedNatsServer {
        url,
        token: before.token,
        nonce: before.nonce,
        owner: None,
    })
}

async fn wait_for_nats_ready(url: &str, token: &str, child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut attempts = 0u32;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("check nats-server status during startup")?
        {
            bail!("nats-server exited during startup with status {status}");
        }

        attempts += 1;
        let result = tokio::time::timeout(
            CONNECT_TIMEOUT,
            async_nats::ConnectOptions::new()
                .token(token.to_owned())
                .connect(url),
        )
        .await;
        match result {
            Ok(Ok(client)) => {
                tokio::time::timeout(CONNECT_TIMEOUT, client.flush())
                    .await
                    .context("timed out flushing shared local NATS readiness connection")??;
                return Ok(());
            }
            Ok(Err(error)) if Instant::now() >= deadline => {
                return Err(error).with_context(|| {
                    format!(
                        "NATS server at {url} did not become ready within {}s after {attempts} attempts",
                        STARTUP_TIMEOUT.as_secs()
                    )
                });
            }
            Err(_) if Instant::now() >= deadline => {
                bail!(
                    "NATS server at {url} did not become ready within {}s after {attempts} attempts: connection timed out",
                    STARTUP_TIMEOUT.as_secs()
                );
            }
            _ => sleep(RETRY_DELAY).await,
        }
    }
}

fn nats_server_binary() -> Result<PathBuf> {
    if let Some(configured) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(configured);
        if !path.is_file() || !is_executable(&path) {
            bail!(
                "NATS_SERVER_BIN points to a missing or non-executable file: {}",
                path.display()
            );
        }
        return absolute_path(&path).with_context(|| {
            format!(
                "NATS_SERVER_BIN points to an unusable nats-server binary: {}",
                path.display()
            )
        });
    }

    match which::which("nats-server") {
        Ok(path) => absolute_path(&path).context("failed to resolve nats-server from PATH"),
        Err(error) => bail!(
            "nats-server binary not found; set NATS_SERVER_BIN to its path or install nats-server on PATH: {error}"
        ),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))
}

fn verify_nats_server_version(binary: &Path) -> Result<()> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {} --version", binary.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version failed with status {}",
            binary.display(),
            output.status
        );
    }
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (major, minor) = parse_major_minor(&text)
        .with_context(|| format!("could not parse nats-server version from {:?}", text.trim()))?;
    if (major, minor) < (2, 11) {
        bail!(
            "nats-server version 2.11 or newer is required; {} reports {}",
            binary.display(),
            text.trim()
        );
    }
    Ok(())
}

fn parse_major_minor(version: &str) -> Option<(u64, u64)> {
    version.split_whitespace().find_map(|word| {
        let candidate = word.trim_matches(|character: char| {
            !character.is_ascii_digit() && character != '.' && character != 'v'
        });
        let candidate = candidate.strip_prefix('v').unwrap_or(candidate);
        let mut parts = candidate.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some((major, minor))
    })
}

/// Write the server config. `port: -1` asks nats-server to take a free port from
/// the kernel and keep it, and `ports_file_dir` is how it reports back what it
/// bound. Allocating the port here and closing the listener before nats-server
/// opened it left a window where another process could claim it, which showed up
/// as a spawn attempt dying immediately.
fn write_server_config_atomically(destination: &Path, token: &str) -> Result<()> {
    let runtime_dir = nats_runtime_dir();
    let store_dir = serde_json::to_string(&nats_runtime_store_dir())
        .context("failed to encode shared local NATS store path")?;
    let ports_dir = serde_json::to_string(&runtime_dir)
        .context("failed to encode shared local NATS ports directory")?;
    let token = serde_json::to_string(token).context("failed to encode shared local NATS token")?;
    let config = format!(
        "host: \"127.0.0.1\"\nport: -1\nports_file_dir: {ports_dir}\njetstream {{ store_dir: {store_dir} }}\nauthorization {{ token: {token} }}\n"
    );
    let mut temporary = NamedTempFile::new_in(&runtime_dir).with_context(|| {
        format!(
            "failed to create temporary NATS config in {}",
            runtime_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("failed to secure temporary NATS config")?;
    }
    temporary
        .as_file_mut()
        .write_all(config.as_bytes())
        .context("failed to write shared local NATS config")?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("failed to sync shared local NATS config")?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to atomically replace {}", destination.display()))?;
    Ok(())
}

fn write_metadata_atomically(metadata: &ServerMetadata) -> Result<()> {
    let runtime_dir = nats_runtime_dir();
    let destination = nats_runtime_ports_file();
    let mut temporary = NamedTempFile::new_in(&runtime_dir).with_context(|| {
        format!(
            "failed to create temporary ports.json in {}",
            runtime_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("failed to secure temporary NATS ports.json")?;
    }
    serde_json::to_writer(temporary.as_file_mut(), metadata)
        .context("failed to serialize shared local NATS ports.json")?;
    temporary
        .as_file_mut()
        .write_all(b"\n")
        .context("failed to finish shared local NATS ports.json")?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("failed to sync shared local NATS ports.json")?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to atomically replace {}", destination.display()))?;
    Ok(())
}

fn read_metadata() -> Result<ServerMetadata> {
    let path = nats_runtime_ports_file();
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "failed to read shared local NATS metadata {}",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to parse shared local NATS metadata {}",
            path.display()
        )
    })
}

fn remove_metadata_if_nonce_matches(nonce: &str) {
    if read_metadata().is_ok_and(|metadata| metadata.nonce == nonce) {
        remove_file_if_present(
            &nats_runtime_ports_file(),
            "stale shared local NATS ports.json",
        );
    }
}

fn remove_file_if_present(path: &Path, description: &str) {
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != ErrorKind::NotFound {
            warn!("failed to remove {description}: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_major_minor, port_report_path};
    use std::path::Path;

    #[test]
    fn ports_report_uses_launched_executable_name() {
        let path = port_report_path(Path::new("custom-nats-server.exe"), 42);

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("custom-nats-server.exe_42.ports")
        );
    }

    #[test]
    fn parses_supported_nats_version_output() {
        assert_eq!(parse_major_minor("nats-server: v2.11.6"), Some((2, 11)));
        assert_eq!(parse_major_minor("nats-server version 3.0.0"), Some((3, 0)));
    }

    #[test]
    fn rejects_unparseable_version_output() {
        assert_eq!(parse_major_minor("nats-server unknown"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_fails_fast_when_child_exits() {
        use super::wait_for_nats_ready;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let mut child = Command::new("sh")
            .args(["-c", "exit 23"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived child");
        let started = Instant::now();

        let error = wait_for_nats_ready("nats://127.0.0.1:1", "unused", &mut child)
            .await
            .expect_err("exited child must fail readiness");

        assert!(
            error
                .to_string()
                .contains("nats-server exited during startup"),
            "unexpected error: {error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "dead child should not wait for startup timeout"
        );
    }
}
