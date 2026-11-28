//! Lifecycle management for the single shared, per-user local NATS server.
//!
//! Ownership is represented by an exclusive file lock. The process holding the
//! lock owns the child server; other processes discover authenticated connection
//! details through an atomically replaced metadata file.

use anyhow::{bail, Context, Result};
use harnx_core::child_process::ChildProcessManager;
use harnx_core::config_paths::{
    nats_runtime_dir, nats_runtime_lock_file, nats_runtime_ports_file, nats_runtime_store_dir,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;
use tempfile::NamedTempFile;
use tokio::time::sleep;
use uuid::Uuid;

mod manager;
mod supervision;
mod wait_notice;

pub(crate) use manager::{cancel_broker_wait, connect_local_broker};
pub use manager::{
    reroute_frontend_broker_notices, restore_frontend_broker_notices, LocalBrokerScope,
};
pub(crate) use supervision::BrokerWatch;
pub use supervision::{LocalBroker, LocalBrokerStatus};
pub use wait_notice::{BrokerNoticeSink, BrokerWaitNotice};

use wait_notice::{WaitNotices, WaitPhase};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);
const RETRY_DELAY: Duration = Duration::from_millis(100);
const SPAWN_ATTEMPTS: usize = 5;
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
        self.owner.as_ref().map(|owner| {
            owner
                .child
                .as_ref()
                .expect("running owner has a child")
                .id()
        })
    }

    // File and child checks are not a readiness proof. A new owner can hold
    // the lock without having published new metadata yet.
    fn owner_or_file_is_current(&mut self) -> bool {
        if let Some(owner) = &mut self.owner {
            return owner
                .child
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        }
        joiner_is_current((&self.url, &self.token, &self.nonce))
    }
}

fn joiner_is_current(cached_identity: (&str, &str, &str)) -> bool {
    let Ok(metadata) = read_metadata() else {
        return false;
    };
    if metadata.nonce.is_empty()
        || (
            metadata.url().as_str(),
            metadata.token.as_str(),
            metadata.nonce.as_str(),
        ) != cached_identity
    {
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
    child: Option<Child>,
    report: Option<PathBuf>,
    config_path: PathBuf,
    _lock_file: File,
    processes: ChildProcessManager,
}

impl ServerOwner {
    fn clear_attempt(&mut self) {
        if let Some(mut child) = self.child.take() {
            stop_server(&mut child);
        }
        if let Some(report) = self.report.take() {
            remove_file_if_present(&report, "shared local NATS ports report");
        }
    }
}

impl Drop for ServerOwner {
    fn drop(&mut self) {
        // Reap first, then release the lifetime lock. Keep ports.json so
        // existing clients can reconnect to the same authenticated endpoint.
        self.clear_attempt();
        remove_file_if_present(&self.config_path, "shared local NATS config");
    }
}

fn stop_server(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
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
    acquire_server(&mut WaitNotices::new(None)).await
}

async fn acquire_server(notices: &mut WaitNotices) -> Result<SharedNatsServer> {
    prepare_runtime_directories()?;
    let lock_file = open_lock_file()?;

    loop {
        if crate::file_lock::try_lock_exclusive(&lock_file)
            .context("failed to acquire shared local NATS lock")?
        {
            return start_owned_server(lock_file, notices).await;
        }

        if let Some(server) = try_join_server().await {
            return Ok(server);
        }

        notices.waiting(WaitPhase::OtherOwner);
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

fn persistent_endpoint() -> Result<(Option<u16>, String)> {
    let previous = match read_metadata() {
        Ok(metadata) => Some(metadata),
        Err(error) if !nats_runtime_ports_file().exists() => {
            log::debug!("initializing shared local NATS endpoint: {error:#}");
            None
        }
        Err(error) => return Err(error).context("read persistent local NATS endpoint"),
    };
    let token = previous.as_ref().map_or_else(
        || Uuid::new_v4().simple().to_string(),
        |metadata| metadata.token.clone(),
    );
    let port = previous.map(|metadata| metadata.port);
    Ok((port, token))
}

async fn start_owned_server(
    lock_file: File,
    notices: &mut WaitNotices,
) -> Result<SharedNatsServer> {
    let binary = nats_server_binary()?;
    let (port, token) = persistent_endpoint()?;
    let mut owner = ServerOwner {
        child: None,
        report: None,
        config_path: nats_runtime_dir().join("nats.conf"),
        _lock_file: lock_file,
        processes: ChildProcessManager::new(),
    };
    verify_nats_server_version(&binary, &mut owner, notices).await?;
    write_server_config_atomically(&owner.config_path, &token, port)?;
    let mut last_error = None;
    for _ in 0..SPAWN_ATTEMPTS {
        match spawn_once(&binary, &mut owner, &token, notices).await {
            Ok(port) => return publish_owned_server(port, token, owner),
            Err(error) => {
                owner.clear_attempt();
                last_error = Some(error);
            }
        }
    }
    Err(last_error.expect("at least one NATS spawn attempt must run")).context(format!(
        "failed to start shared local NATS server after {SPAWN_ATTEMPTS} attempts"
    ))
}

fn publish_owned_server(port: u16, token: String, owner: ServerOwner) -> Result<SharedNatsServer> {
    let metadata = ServerMetadata {
        port,
        nonce: Uuid::new_v4().to_string(),
        token,
    };
    write_metadata_atomically(&metadata)?;
    Ok(SharedNatsServer {
        url: metadata.url(),
        token: metadata.token,
        nonce: metadata.nonce,
        owner: Some(owner),
    })
}

async fn spawn_once(
    binary: &Path,
    owner: &mut ServerOwner,
    token: &str,
    notices: &mut WaitNotices,
) -> Result<u16> {
    let mut command = Command::new(binary);
    command
        .arg("-c")
        .arg(&owner.config_path)
        .stdin(Stdio::null())
        .stdout(harnx_core::logging::child_output_sink())
        .stderr(harnx_core::logging::child_output_sink());
    let child = owner
        .processes
        .spawn_std(command)
        .with_context(|| format!("failed to spawn {}", binary.display()))?;
    let report = port_report_path(binary, child.id());
    owner.report = Some(report.clone());
    let child = owner.child.insert(child);
    let port = read_bound_port(child, &report, notices).await?;
    let url = format!("nats://127.0.0.1:{port}");
    wait_for_nats_ready(child, notices, || async {
        connect_authenticated(&url, token).await.map(|_| ())
    })
    .await?;
    remove_file_if_present(&report, "shared local NATS ports report");
    owner.report.take();
    Ok(port)
}

/// Wait for nats-server to report the port it bound.
///
/// nats-server writes into the file rather than renaming it into place, so a
/// parse failure is treated the same as not-yet-written.
async fn read_bound_port(child: &mut Child, path: &Path, notices: &mut WaitNotices) -> Result<u16> {
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("shared local NATS exited during startup: {status}");
        }
        if let Some(port) = parse_reported_port(path) {
            return Ok(port);
        }
        notices.waiting(WaitPhase::PortReport(child.id()));
        sleep(CHILD_POLL_INTERVAL).await;
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

async fn try_join_server() -> Option<SharedNatsServer> {
    let before = read_metadata().ok()?;
    let url = before.url();
    connect_authenticated(&url, &before.token).await.ok()?;
    if !joiner_is_current((&url, &before.token, &before.nonce)) {
        return None;
    }
    Some(SharedNatsServer {
        url,
        token: before.token,
        nonce: before.nonce,
        owner: None,
    })
}

async fn connect_authenticated(url: &str, token: &str) -> Result<async_nats::Client> {
    let options = harnx_nats_common::connect::managed_local_connect_options(token.to_owned());
    // The dial future embeds TLS machinery even for plaintext; don't store
    // that frame inline in every caller of the managed-local readiness path.
    let client = tokio::time::timeout(CONNECT_TIMEOUT, Box::pin(options.connect(url)))
        .await
        .context("timed out connecting to shared local NATS")??;
    flush_probe(client.flush()).await?;
    // A plaintext server without auth ignores the token entirely. It is not
    // the elected local broker, even if it occupies the recorded port.
    if !client
        .try_server_info()
        .is_some_and(|info| info.auth_required && info.jetstream)
    {
        bail!("the shared local NATS endpoint must require authentication and offer JetStream");
    }
    Ok(client)
}

async fn flush_probe<E>(flush: impl std::future::Future<Output = Result<(), E>>) -> Result<()>
where
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::time::timeout(CONNECT_TIMEOUT, flush)
        .await
        .context("timed out flushing shared local NATS readiness connection")??;
    Ok(())
}

async fn wait_for_nats_ready<F: std::future::Future<Output = Result<()>>>(
    child: &mut Child,
    notices: &mut WaitNotices,
    mut probe: impl FnMut() -> F,
) -> Result<()> {
    loop {
        if let Some(status) = child
            .try_wait()
            .context("check nats-server status during startup")?
        {
            bail!("nats-server exited during startup with status {status}");
        }
        if probe().await.is_ok() {
            return Ok(());
        }
        notices.waiting(WaitPhase::Readiness(child.id()));
        sleep(RETRY_DELAY).await;
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

async fn verify_nats_server_version(
    binary: &Path,
    owner: &mut ServerOwner,
    notices: &mut WaitNotices,
) -> Result<()> {
    use std::io::Read as _;
    // Use a file so checking version remains cancellable without a separate
    // pipe reader task. The owner guard reaps this child too on shutdown.
    let mut output = tempfile::tempfile().context("open nats-server version output")?;
    let mut command = Command::new(binary);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output.try_clone()?);
    let child = owner
        .processes
        .spawn_std(command)
        .with_context(|| format!("failed to run {} --version", binary.display()))?;
    let child = owner.child.insert(child);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        notices.waiting(WaitPhase::Version(child.id()));
        sleep(CHILD_POLL_INTERVAL).await;
    };
    owner.child.take();
    if !status.success() {
        bail!("{} --version failed with status {status}", binary.display());
    }
    use std::io::Seek as _;
    output.rewind()?;
    let mut text = String::new();
    output.read_to_string(&mut text)?;
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
fn write_server_config_atomically(
    destination: &Path,
    token: &str,
    port: Option<u16>,
) -> Result<()> {
    let runtime_dir = nats_runtime_dir();
    let store_dir = serde_json::to_string(&nats_runtime_store_dir())
        .context("failed to encode shared local NATS store path")?;
    let ports_dir = serde_json::to_string(&runtime_dir)
        .context("failed to encode shared local NATS ports directory")?;
    let token = serde_json::to_string(token).context("failed to encode shared local NATS token")?;
    // Only first startup asks the broker to allocate a port. Failover must bind
    // the original endpoint, never silently strand clients on another port.
    let port = port.map_or(-1, i32::from);
    let config = format!(
        "host: \"127.0.0.1\"\nport: {port}\nports_file_dir: {ports_dir}\njetstream {{ store_dir: {store_dir} }}\nauthorization {{ token: {token} }}\n"
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

        let error = wait_for_nats_ready(&mut child, &mut super::WaitNotices::new(None), || async {
            super::connect_authenticated("nats://127.0.0.1:1", "unused")
                .await
                .map(|_| ())
        })
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

#[cfg(test)]
#[cfg(unix)]
mod probe_tests {
    use super::*;

    struct TestChild(Child);

    impl Drop for TestChild {
        fn drop(&mut self) {
            stop_server(&mut self.0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn nats_readiness_retries_connect_and_flush_failures_on_same_child() {
        let processes = ChildProcessManager::new();
        let mut command = Command::new("sleep");
        command.arg("60");
        let mut child = TestChild(processes.spawn_std(command).unwrap());
        let pid = child.0.id();
        let mut attempts = 0;
        let mut notices = WaitNotices::new(None);
        wait_for_nats_ready(&mut child.0, &mut notices, || {
            attempts += 1;
            controlled_probe(attempts)
        })
        .await
        .unwrap();
        assert_eq!(attempts, 4);
        assert_eq!(child.0.id(), pid);
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "a probe failure killed the child"
        );
    }

    async fn controlled_probe(attempt: usize) -> Result<()> {
        use std::io;
        match attempt {
            1 => Err(io::Error::from(ErrorKind::ConnectionRefused).into()),
            2 => flush_probe(std::future::pending::<io::Result<()>>()).await,
            3 => {
                flush_probe(std::future::ready(Err(io::Error::from(
                    ErrorKind::BrokenPipe,
                ))))
                .await
            }
            4 => flush_probe(std::future::ready(Ok::<_, io::Error>(()))).await,
            _ => panic!("readiness retried after success"),
        }
    }
}
