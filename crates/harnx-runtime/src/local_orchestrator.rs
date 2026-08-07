//! Shared-local worker subprocess supervision.
//!
//! One front-end owns the worker through `worker.lock`; other front-ends join
//! the same broker and wait for that worker's readiness heartbeat.

use crate::config::{
    HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV, HARNX_WORKER_BIN_ENV, LOCAL_CLUSTER_KEY,
};
use crate::nats_local_server::{ensure_shared_server, SharedNatsServer};
use crate::nats_worker::worker_ready_subject;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use harnx_core::abort::AbortSignal;
use harnx_core::config_paths::{nats_runtime_dir, state_path};
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::sink::emit_agent_event;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const READINESS_POLL_INITIAL: Duration = Duration::from_millis(50);
const READINESS_POLL_MAX: Duration = Duration::from_millis(500);
const JOINER_READY_TTL: Duration = Duration::from_secs(3);
const LOCAL_WORKER_ID: &str = "local";
/// How long the worker may take before the user is told it is still starting.
const WORKER_SLOW_NOTICE_AFTER: Duration = Duration::from_secs(5);
/// Spacing of the reminders that follow the first one.
const WORKER_SLOW_NOTICE_INTERVAL: Duration = Duration::from_secs(10);
/// Consecutive worker exits tolerated before the wait is declared hopeless.
const MAX_WORKER_CRASHES: u32 = 3;
/// Bytes of worker output shown when startup gives up.
const WORKER_OUTPUT_TAIL_BYTES: u64 = 4096;

/// Name of the worker executable front-ends spawn.
const WORKER_BINARY: &str = if cfg!(windows) {
    "harnx-worker.exe"
} else {
    "harnx-worker"
};

/// Path used to elect one local worker owner per user/broker.
pub fn local_worker_lock_file() -> PathBuf {
    nats_runtime_dir().join("worker.lock")
}

/// Locate the `harnx-worker` binary a front-end should spawn.
///
/// `HARNX_WORKER_BIN` wins, then a sibling of the running front-end, then
/// `PATH`. The sibling case is what makes an ordinary install work: front-end
/// and worker land in the same `bin` directory, so neither has to be on `PATH`.
pub fn resolve_worker_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(HARNX_WORKER_BIN_ENV) {
        let path = PathBuf::from(path);
        if !path.is_file() {
            bail!(
                "{HARNX_WORKER_BIN_ENV} points at {}, which is not a file",
                path.display()
            );
        }
        return Ok(path);
    }

    let current = std::env::current_exe().context("resolve current frontend executable")?;
    let directory = current
        .parent()
        .context("current frontend executable has no parent directory")?;
    // Integration tests run from `target/debug/deps`; the worker sits one level up.
    let directory = if directory.file_name().is_some_and(|name| name == "deps") {
        directory
            .parent()
            .context("test executable deps directory has no parent")?
    } else {
        directory
    };
    let sibling = directory.join(WORKER_BINARY);
    if sibling.is_file() {
        return Ok(sibling);
    }

    which::which(WORKER_BINARY).with_context(|| {
        format!(
            "worker binary '{WORKER_BINARY}' not found next to {} or on PATH; \
             install it or set {HARNX_WORKER_BIN_ENV}",
            current.display()
        )
    })
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
    /// Owned workers that exited during the current readiness wait.
    crashes: u32,
}

/// Lazily starts or re-checks a front-end's process-lifetime local worker.
///
/// Front-ends keep the `Option` alive for their own lifetime. Repeated calls are
/// cheap and respawn an owned worker when it has exited.
pub async fn ensure_local_worker(
    supervisor: &mut Option<LocalWorkerSupervisor>,
    abort_signal: AbortSignal,
) -> Result<()> {
    match supervisor {
        Some(supervisor) => supervisor.ensure(abort_signal).await,
        slot @ None => {
            *slot = Some(LocalWorkerSupervisor::start(abort_signal).await?);
            Ok(())
        }
    }
}

impl LocalWorkerSupervisor {
    /// Ensure the shared broker and local worker using the `harnx-worker`
    /// executable found next to the running front-end.
    pub async fn start(abort_signal: AbortSignal) -> Result<Self> {
        Self::start_with_worker_binary(resolve_worker_binary()?, abort_signal).await
    }

    /// Ensure the shared broker and local worker using an explicit
    /// `harnx-worker` binary. Integration tests use this when the worker they
    /// want is not the one discovery would pick.
    pub async fn start_with_worker_binary(
        binary: impl AsRef<Path>,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let server = ensure_shared_server().await?;
        let worker_binary = binary
            .as_ref()
            .canonicalize()
            .with_context(|| format!("resolve worker binary {}", binary.as_ref().display()))?;
        let lock_file = open_worker_lock()?;
        let owns_lock = crate::file_lock::try_lock_exclusive(&lock_file)
            .context("acquire local worker lock")?;
        let mut supervisor = Self {
            server,
            worker_binary,
            lock_file: Some(lock_file),
            owns_lock,
            child: None,
            last_confirmed_ready: None,
            crashes: 0,
        };
        supervisor.ensure(abort_signal).await?;
        Ok(supervisor)
    }

    /// Check worker health, respawn an exited owned worker, or take ownership
    /// after another front-end releases `worker.lock`. Returns only after a
    /// post-consumer readiness marker is observed.
    pub async fn ensure(&mut self, abort_signal: AbortSignal) -> Result<()> {
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
        self.crashes = 0;
        self.ensure_worker_ownership()?;
        self.wait_for_readiness(readiness, abort_signal).await
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
        let acquired = crate::file_lock::try_lock_exclusive(
            self.lock_file
                .as_ref()
                .expect("worker lock file must exist while supervisor is alive"),
        )
        .context("retry local worker lock")?;
        if acquired {
            self.owns_lock = true;
            self.spawn_worker()?;
        }
        Ok(())
    }

    /// Wait for the worker's readiness heartbeat, retrying until it arrives or
    /// the user aborts.
    ///
    /// A worker that is merely slow gets unlimited time — startup cost scales
    /// with the user's agent and tool-server count, so any fixed deadline is
    /// wrong for someone. A worker that keeps *exiting* is a different failure:
    /// retrying cannot help, so give up after [`MAX_WORKER_CRASHES`] and show
    /// what it printed.
    async fn wait_for_readiness(
        &mut self,
        mut readiness: async_nats::Subscriber,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let started = Instant::now();
        let mut poll = READINESS_POLL_INITIAL;
        let mut next_notice = WORKER_SLOW_NOTICE_AFTER;
        loop {
            if abort_signal.aborted() {
                bail!("cancelled while waiting for the local worker to start");
            }
            match tokio::time::timeout(poll, readiness.next()).await {
                Ok(Some(_)) => {
                    self.last_confirmed_ready = Some(Instant::now());
                    return Ok(());
                }
                Ok(None) => bail!("local worker readiness subscription closed"),
                Err(_) => self.ensure_worker_ownership()?,
            }

            if self.crashes >= MAX_WORKER_CRASHES {
                bail!(
                    "local worker exited {} times without becoming ready:\n{}",
                    self.crashes,
                    worker_output_tail()
                );
            }

            let waited = started.elapsed();
            if waited >= next_notice {
                emit_worker_wait_notice(waited);
                next_notice = waited + WORKER_SLOW_NOTICE_INTERVAL;
            }
            poll = (poll * 2).min(READINESS_POLL_MAX);
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
                self.crashes = self.crashes.saturating_add(1);
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
            .arg("--cluster")
            .arg(LOCAL_CLUSTER_KEY)
            .arg("--worker-id")
            .arg(LOCAL_WORKER_ID)
            .env(HARNX_NATS_URL_ENV, &self.server.url)
            .env(HARNX_NATS_TOKEN_ENV, &self.server.token)
            .stdin(Stdio::null())
            .stdout(worker_output_sink())
            .stderr(worker_output_sink())
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

/// File the local worker's stdout/stderr is appended to.
///
/// The worker is a detached subprocess with no terminal, so anything it writes
/// outside the `log` facade — a panic, a `main` returning `Err`, a child
/// process's own stderr — is otherwise lost. That made startup failures
/// undiagnosable: the front-end only saw a readiness timeout.
pub fn local_worker_output_file() -> PathBuf {
    state_path("harnx_worker.log")
}

/// Tell the user the worker is still coming up, on the same channel as other
/// agent notices so the TUI and the CLI both surface it.
fn emit_worker_wait_notice(waited: Duration) {
    let message = format!(
        "Still waiting for the local worker to start ({}s). Ctrl-C to cancel; worker output goes to {}.",
        waited.as_secs(),
        local_worker_output_file().display()
    );
    // Warning, not Info: the CLI sink routes Info to stdout, where progress
    // chatter would corrupt piped output from a one-shot invocation.
    emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning(message.clone())));
    log::info!("{message}");
}

/// Last [`WORKER_OUTPUT_TAIL_BYTES`] of the worker log, for error messages.
fn worker_output_tail() -> String {
    let path = local_worker_output_file();
    let render = |body: String| {
        if body.trim().is_empty() {
            format!("(no output in {})", path.display())
        } else {
            format!("--- tail of {} ---\n{}", path.display(), body.trim_end())
        }
    };
    let Ok(mut file) = File::open(&path) else {
        return render(String::new());
    };
    let Ok(length) = file.metadata().map(|meta| meta.len()) else {
        return render(String::new());
    };
    let start = length.saturating_sub(WORKER_OUTPUT_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return render(String::new());
    }
    // Decode leniently: an arbitrary byte offset can land mid-sequence, and
    // `read_to_string` would reject the whole read for one split character —
    // discarding exactly the output this message exists to show.
    let mut body = Vec::new();
    let _ = file.read_to_end(&mut body);
    render(String::from_utf8_lossy(&body).into_owned())
}

/// Append-mode handle to [`local_worker_output_file`], falling back to a null
/// sink so a non-writable state dir never blocks worker startup.
///
/// Also used for the worker's own children (tool and hook servers) so the whole
/// worker subtree explains itself in one file. They redirect to this path rather
/// than inheriting the worker's descriptors: an inherited pipe outlives the
/// child that holds it, which strands test harness output.
pub(crate) fn worker_output_sink() -> Stdio {
    let path = local_worker_output_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Stdio::from(file),
        Err(error) => {
            log::warn!(
                "local worker output not captured to {}: {error}",
                path.display()
            );
            Stdio::null()
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `HARNX_WORKER_BIN` wins over sibling/PATH discovery. Nextest gives each
    /// test its own process, so setting the variable here is contained.
    #[test]
    fn worker_bin_override_wins() {
        harnx_core::require_nextest();
        let file = tempfile::NamedTempFile::new().expect("create fake worker binary");
        unsafe { std::env::set_var(HARNX_WORKER_BIN_ENV, file.path()) };
        assert_eq!(resolve_worker_binary().unwrap(), file.path());
    }

    /// A stale override must fail loudly instead of silently falling back to a
    /// different worker than the operator asked for.
    #[test]
    fn worker_bin_override_rejects_missing_file() {
        harnx_core::require_nextest();
        unsafe { std::env::set_var(HARNX_WORKER_BIN_ENV, "/nonexistent/harnx-worker") };
        let error = resolve_worker_binary().expect_err("missing override must fail");
        assert!(
            error.to_string().contains("/nonexistent/harnx-worker"),
            "unexpected error: {error:#}"
        );
    }
}
