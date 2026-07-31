//! Shared-local worker subprocess supervision.
//!
//! One front-end owns the worker through `worker.lock`; other front-ends join
//! the same broker and wait for that worker's readiness heartbeat.

use crate::config::{HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV, LOCAL_CLUSTER_KEY};
use crate::nats_local_server::{ensure_shared_server, SharedNatsServer};
use crate::nats_worker::worker_ready_subject;
use anyhow::{bail, Context, Result};
use fs4::fs_std::FileExt;
use futures_util::StreamExt;
use harnx_core::config_paths::nats_runtime_dir;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const READINESS_POLL_INITIAL: Duration = Duration::from_millis(50);
const READINESS_POLL_MAX: Duration = Duration::from_millis(500);
const JOINER_READY_TTL: Duration = Duration::from_secs(3);
const LOCAL_WORKER_ID: &str = "local";

/// Path used to elect one local worker owner per user/broker.
pub fn local_worker_lock_file() -> PathBuf {
    nats_runtime_dir().join("worker.lock")
}

/// Keeps the shared broker alive and owns a worker subprocess when this
/// front-end wins `worker.lock`.
pub struct LocalWorkerSupervisor {
    server: SharedNatsServer,
    worker_binary: PathBuf,
    lock_file: Option<File>,
    owns_lock: bool,
    child: Option<Child>,
    last_confirmed_ready: Option<Instant>,
}

/// Lazily starts or re-checks a front-end's process-lifetime local worker.
///
/// Front-ends keep the `Option` alive for their own lifetime. Repeated calls are
/// cheap and respawn an owned worker when it has exited.
pub async fn ensure_local_worker(supervisor: &mut Option<LocalWorkerSupervisor>) -> Result<()> {
    match supervisor {
        Some(supervisor) => supervisor.ensure().await,
        slot @ None => {
            *slot = Some(LocalWorkerSupervisor::start().await?);
            Ok(())
        }
    }
}

impl LocalWorkerSupervisor {
    /// Ensure the shared broker and local worker using the `harnx` executable.
    /// Unified CLI callers use their current executable; standalone frontends
    /// such as `harnx-serve` resolve `HARNX_BIN` or a sibling
    /// `harnx` binary because they do not expose the `worker` subcommand.
    pub async fn start() -> Result<Self> {
        let current = std::env::current_exe().context("resolve current frontend executable")?;
        let is_harnx = current
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "harnx");
        let binary = if is_harnx {
            current
        } else if let Some(path) = std::env::var_os("HARNX_BIN") {
            PathBuf::from(path)
        } else {
            let sibling = current.with_file_name(if cfg!(windows) { "harnx.exe" } else { "harnx" });
            if !sibling.is_file() {
                bail!(
                    "frontend {} requires HARNX_BIN or sibling harnx binary to start local worker",
                    current.display()
                );
            }
            sibling
        };
        Self::start_with_worker_binary(binary).await
    }

    /// Ensure the shared broker and local worker using an explicit `harnx`
    /// binary. Integration tests and front-end-specific binaries may use this
    /// when their current executable does not expose the `worker` subcommand.
    pub async fn start_with_worker_binary(binary: impl AsRef<Path>) -> Result<Self> {
        let server = ensure_shared_server().await?;
        let worker_binary = binary
            .as_ref()
            .canonicalize()
            .with_context(|| format!("resolve worker binary {}", binary.as_ref().display()))?;
        let lock_file = open_worker_lock()?;
        let owns_lock = lock_file
            .try_lock_exclusive()
            .context("acquire local worker lock")?;
        let mut supervisor = Self {
            server,
            worker_binary,
            lock_file: Some(lock_file),
            owns_lock,
            child: None,
            last_confirmed_ready: None,
        };
        supervisor.ensure().await?;
        Ok(supervisor)
    }

    /// Check worker health, respawn an exited owned worker, or take ownership
    /// after another front-end releases `worker.lock`. Returns only after a
    /// post-consumer readiness marker is observed.
    pub async fn ensure(&mut self) -> Result<()> {
        if self.owned_child_is_running()? {
            return Ok(());
        }
        if !self.owns_lock
            && self
                .last_confirmed_ready
                .is_some_and(|ready| ready.elapsed() < JOINER_READY_TTL)
        {
            return Ok(());
        }

        let readiness = self.subscribe_to_readiness().await?;
        self.ensure_worker_ownership()?;
        self.wait_for_readiness(readiness).await
    }

    async fn subscribe_to_readiness(&self) -> Result<async_nats::Subscriber> {
        let client = async_nats::ConnectOptions::new()
            .token(self.server.token.clone())
            .connect(&self.server.url)
            .await
            .context("connect local worker readiness client")?;
        let readiness = client
            .subscribe(worker_ready_subject(LOCAL_CLUSTER_KEY))
            .await
            .context("subscribe to local worker readiness")?;
        client
            .flush()
            .await
            .context("flush local worker readiness subscription")?;
        Ok(readiness)
    }

    fn ensure_worker_ownership(&mut self) -> Result<()> {
        if self.owns_lock {
            if !self.owned_child_is_running()? {
                self.spawn_worker()?;
            }
            return Ok(());
        }
        let acquired = self
            .lock_file
            .as_ref()
            .expect("worker lock file must exist while supervisor is alive")
            .try_lock_exclusive()
            .context("retry local worker lock")?;
        if acquired {
            self.owns_lock = true;
            self.spawn_worker()?;
        }
        Ok(())
    }

    async fn wait_for_readiness(&mut self, mut readiness: async_nats::Subscriber) -> Result<()> {
        let deadline = Instant::now() + WORKER_STARTUP_TIMEOUT;
        let mut poll = READINESS_POLL_INITIAL;
        loop {
            match tokio::time::timeout(poll, readiness.next()).await {
                Ok(Some(_)) => {
                    self.last_confirmed_ready = Some(Instant::now());
                    return Ok(());
                }
                Ok(None) => bail!("local worker readiness subscription closed"),
                Err(_) => self.ensure_worker_ownership()?,
            }
            poll = (poll * 2).min(READINESS_POLL_MAX);
            if Instant::now() >= deadline {
                bail!(
                    "local worker did not publish readiness on {} within {}s",
                    worker_ready_subject(LOCAL_CLUSTER_KEY),
                    WORKER_STARTUP_TIMEOUT.as_secs()
                );
            }
        }
    }

    /// Whether this front-end owns `worker.lock` and therefore the subprocess.
    pub fn is_worker_owner(&self) -> bool {
        self.owns_lock
    }

    /// PID of this supervisor's worker, if it owns one.
    pub fn worker_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Shared broker connection details retained for supervisor lifetime.
    pub fn server(&self) -> &SharedNatsServer {
        &self.server
    }

    fn owned_child_is_running(&mut self) -> Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        match child.try_wait().context("check local worker status")? {
            None => Ok(true),
            Some(status) => {
                log::warn!("local worker exited with {status}; respawning");
                self.child = None;
                Ok(false)
            }
        }
    }

    fn spawn_worker(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let mut command = Command::new(&self.worker_binary);
        command
            .arg("worker")
            .arg("--cluster")
            .arg(LOCAL_CLUSTER_KEY)
            .arg("--worker-id")
            .arg(LOCAL_WORKER_ID)
            .env(HARNX_NATS_URL_ENV, &self.server.url)
            .env(HARNX_NATS_TOKEN_ENV, &self.server.token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        configure_worker_process(&mut command);
        self.child = Some(command.spawn().with_context(|| {
            format!("spawn local worker from {}", self.worker_binary.display())
        })?);
        Ok(())
    }
}

impl Drop for LocalWorkerSupervisor {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            // Negative PID addresses the worker's dedicated process group.
            // SAFETY: kill is async-signal-safe and the PID came from Child.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
            }
        }
        let _ = child.start_kill();

        // Keep the lock descriptor alive until the child is reaped. This prevents
        // another front-end from spawning a replacement while shutdown is still
        // in progress, without blocking the caller's async runtime thread.
        let lock_file = self.owns_lock.then(|| {
            self.lock_file
                .take()
                .expect("worker owner must retain its lock file")
        });
        let _ = std::thread::Builder::new()
            .name("harnx-worker-reaper".to_string())
            .spawn(move || {
                let _lock_file = lock_file;
                while matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
    }
}

fn open_worker_lock() -> Result<File> {
    let path = local_worker_lock_file();
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&path)
        .with_context(|| format!("open local worker lock {}", path.display()))
}

#[cfg(unix)]
fn configure_worker_process(command: &mut Command) {
    #[cfg(target_os = "linux")]
    let parent_pid = std::process::id() as libc::pid_t;
    // SAFETY: pre_exec invokes only async-signal-safe libc calls.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    libc::raise(libc::SIGTERM);
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_worker_process(_command: &mut Command) {}
