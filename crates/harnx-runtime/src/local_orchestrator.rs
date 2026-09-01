//! Frontend-owned local worker subprocess supervision.
//!
//! Frontends share the local NATS broker and durable session state, but each
//! supervisor owns exactly one targeted worker for its process lifetime.

use crate::config::{
    HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV, HARNX_WORKER_BIN_ENV, LOCAL_CLUSTER_KEY,
};
use crate::nats_local_server::{ensure_shared_server, SharedNatsServer};
use crate::nats_worker::{
    targeted_worker_ready_subject, validate_worker_id, LocalWorkerTarget, SessionActivationRoute,
};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::sink::emit_agent_event;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const READINESS_POLL_INITIAL: Duration = Duration::from_millis(50);
const READINESS_POLL_MAX: Duration = Duration::from_millis(500);
/// A healthy worker publishes readiness every 250ms. Waiting for four missed
/// markers distinguishes an event-loop stall from ordinary scheduler jitter.
const WORKER_HEALTH_TIMEOUT: Duration = Duration::from_secs(1);
const WORKER_SLOW_NOTICE_AFTER: Duration = Duration::from_secs(5);
const WORKER_SLOW_NOTICE_INTERVAL: Duration = Duration::from_secs(10);
const MAX_WORKER_CRASHES: u32 = 3;
const WORKER_OUTPUT_TAIL_BYTES: u64 = 4096;

const WORKER_BINARY: &str = if cfg!(windows) {
    "harnx-worker.exe"
} else {
    "harnx-worker"
};

/// The stable activation target owned by one frontend supervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalWorkerRoute {
    session_scope: String,
    worker_id: String,
}

impl LocalWorkerRoute {
    fn new() -> Self {
        Self {
            session_scope: LOCAL_CLUSTER_KEY.to_string(),
            worker_id: format!("local-{}", uuid::Uuid::new_v4()),
        }
    }

    pub fn session_scope(&self) -> &str {
        &self.session_scope
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub fn activation_route(&self) -> SessionActivationRoute {
        SessionActivationRoute::WorkerTargeted {
            session_scope: self.session_scope.clone(),
            worker_id: self.worker_id.clone(),
        }
    }
}

/// Locate the `harnx-worker` binary a frontend should spawn.
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

/// Keeps the shared broker alive and owns one frontend-affine worker process.
pub struct LocalWorkerSupervisor {
    server: SharedNatsServer,
    worker_binary: PathBuf,
    route: LocalWorkerRoute,
    child: Option<Child>,
    crashes: u32,
}

/// Lazily start or re-check a frontend's process-lifetime local worker and
/// return its stable activation route.
pub async fn ensure_local_worker(
    supervisor: &mut Option<LocalWorkerSupervisor>,
    abort_signal: AbortSignal,
) -> Result<LocalWorkerRoute> {
    match supervisor {
        Some(supervisor) => supervisor.ensure(abort_signal).await,
        slot @ None => {
            let started = LocalWorkerSupervisor::start(abort_signal).await?;
            let route = started.route().clone();
            *slot = Some(started);
            Ok(route)
        }
    }
}

/// Resolve how a frontend should activate sessions on `cluster`, starting its
/// frontend-owned worker only for the reserved local scope.
pub async fn activation_route_for_cluster(
    cluster: &str,
    supervisor: &tokio::sync::Mutex<Option<LocalWorkerSupervisor>>,
    abort_signal: AbortSignal,
) -> Result<SessionActivationRoute> {
    if cluster != LOCAL_CLUSTER_KEY {
        return Ok(SessionActivationRoute::ClusterShared);
    }
    let mut supervisor = supervisor.lock().await;
    ensure_local_worker(&mut supervisor, abort_signal)
        .await
        .context("failed to ensure local NATS worker")
        .map(|route| route.activation_route())
}

impl LocalWorkerSupervisor {
    pub async fn start(abort_signal: AbortSignal) -> Result<Self> {
        Self::start_with_worker_binary(resolve_worker_binary()?, abort_signal).await
    }

    pub async fn start_with_worker_binary(
        binary: impl AsRef<Path>,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let server = ensure_shared_server().await?;
        let worker_binary = binary
            .as_ref()
            .canonicalize()
            .with_context(|| format!("resolve worker binary {}", binary.as_ref().display()))?;
        let route = LocalWorkerRoute::new();
        validate_worker_id(route.worker_id())?;
        let mut supervisor = Self {
            server,
            worker_binary,
            route,
            child: None,
            crashes: 0,
        };
        supervisor.ensure(abort_signal).await?;
        Ok(supervisor)
    }

    /// Check health and respawn this supervisor's worker after a crash or
    /// event-loop stall.
    /// Running workers are never restarted for binary or configuration changes.
    pub async fn ensure(&mut self, abort_signal: AbortSignal) -> Result<LocalWorkerRoute> {
        if self
            .server
            .refresh_if_stale()
            .await
            .context("refresh shared local NATS server")?
        {
            // A worker is affine to the broker identity passed in its
            // environment. It cannot serve turns on the replacement broker,
            // even if its process has not noticed the old broker's exit yet.
            self.stop_worker();
        }

        // Subscribe before checking or spawning. Core NATS does not replay old
        // readiness markers, so a marker received below proves the existing
        // worker's event loop is currently making progress. The same
        // subscription then admits a replacement without a spawn/subscribe
        // race.
        let mut readiness = self.subscribe_to_readiness().await?;
        if self.child_is_running()? {
            let expected_pid = self
                .worker_pid()
                .context("running local worker has no PID")?;
            if self
                .wait_for_health_marker(&mut readiness, expected_pid, abort_signal.clone())
                .await?
            {
                return Ok(self.route.clone());
            }
            log::warn!(
                "local worker {} pid={} stopped reporting readiness; respawning",
                self.route.worker_id(),
                expected_pid,
            );
            self.stop_worker();
        }

        self.crashes = 0;
        let expected_pid = self.spawn_worker()?;
        self.wait_for_readiness(&mut readiness, expected_pid, abort_signal)
            .await
    }

    async fn wait_for_health_marker(
        &mut self,
        readiness: &mut async_nats::Subscriber,
        expected_pid: u32,
        abort_signal: AbortSignal,
    ) -> Result<bool> {
        let deadline = tokio::time::sleep(WORKER_HEALTH_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = wait_abort_signal(&abort_signal) => {
                    bail!("cancelled while checking the local worker");
                }
                _ = &mut deadline => return Ok(false),
                message = readiness.next() => {
                    let message = message.context("local worker readiness subscription closed")?;
                    if self
                        .accept_readiness(&message.payload, expected_pid)?
                        .is_some()
                    {
                        return self.child_is_running();
                    }
                }
            }
        }
    }

    async fn wait_for_readiness(
        &mut self,
        readiness: &mut async_nats::Subscriber,
        mut expected_pid: u32,
        abort_signal: AbortSignal,
    ) -> Result<LocalWorkerRoute> {
        let started = Instant::now();
        let mut poll = READINESS_POLL_INITIAL;
        let mut next_notice = WORKER_SLOW_NOTICE_AFTER;
        loop {
            if abort_signal.aborted() {
                bail!("cancelled while waiting for the local worker to start");
            }

            if let Some(route) = self
                .poll_readiness(readiness, poll, &mut expected_pid)
                .await?
            {
                return Ok(route);
            }

            let waited = started.elapsed();
            if waited >= next_notice {
                emit_worker_wait_notice(waited, expected_pid);
                next_notice = waited + WORKER_SLOW_NOTICE_INTERVAL;
            }
            poll = (poll * 2).min(READINESS_POLL_MAX);
        }
    }

    async fn poll_readiness(
        &mut self,
        readiness: &mut async_nats::Subscriber,
        poll: Duration,
        expected_pid: &mut u32,
    ) -> Result<Option<LocalWorkerRoute>> {
        match tokio::time::timeout(poll, readiness.next()).await {
            Ok(Some(message)) => self.accept_readiness(&message.payload, *expected_pid),
            Ok(None) => bail!("local worker readiness subscription closed"),
            Err(_) => {
                self.respawn_if_needed(expected_pid)?;
                Ok(None)
            }
        }
    }

    fn accept_readiness(
        &self,
        payload: &[u8],
        expected_pid: u32,
    ) -> Result<Option<LocalWorkerRoute>> {
        let record = crate::worker_identity::WorkerReadiness::from_payload(payload)
            .and_then(|record| {
                record
                    .validate_route(self.route.session_scope(), self.route.worker_id())
                    .map(|()| record)
            })
            .with_context(|| {
                format!(
                    "reject readiness from local worker {}",
                    self.route.worker_id()
                )
            })?;
        if !record.has_pid(expected_pid) {
            log::debug!(
                "ignoring stale local worker readiness: worker_id={} expected_pid={} marker_pid={}",
                self.route.worker_id(),
                expected_pid,
                record.pid,
            );
            return Ok(None);
        }
        log::info!(
            "local worker ready: session_scope={} worker_id={} pid={} build={}",
            record.session_scope,
            record.worker_id,
            record.pid,
            record.build,
        );
        Ok(Some(self.route.clone()))
    }

    fn respawn_if_needed(&mut self, expected_pid: &mut u32) -> Result<()> {
        if self.child_is_running()? {
            return Ok(());
        }
        if self.crashes >= MAX_WORKER_CRASHES {
            bail!(
                "local worker exited {} times without becoming ready:\n{}",
                self.crashes,
                worker_output_tail()
            );
        }
        *expected_pid = self.spawn_worker()?;
        Ok(())
    }

    async fn subscribe_to_readiness(&self) -> Result<async_nats::Subscriber> {
        let client = async_nats::ConnectOptions::new()
            .token(self.server.token.clone())
            .connect(&self.server.url)
            .await
            .context("connect local worker readiness client")?;
        let subject = targeted_worker_ready_subject(LocalWorkerTarget::new(
            self.route.session_scope(),
            self.route.worker_id(),
        )?);
        let readiness = client
            .subscribe(subject)
            .await
            .context("subscribe to targeted local worker readiness")?;
        client
            .flush()
            .await
            .context("flush local worker readiness subscription")?;
        Ok(readiness)
    }

    fn child_is_running(&mut self) -> Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        match child.try_wait().context("check local worker status")? {
            None => Ok(true),
            Some(status) => {
                log::warn!(
                    "local worker {} exited with {status}; respawning",
                    self.route.worker_id()
                );
                self.child = None;
                self.crashes = self.crashes.saturating_add(1);
                Ok(false)
            }
        }
    }

    fn spawn_worker(&mut self) -> Result<u32> {
        debug_assert!(self.child.is_none());
        let mut command = build_local_worker_command(
            &self.worker_binary,
            self.route.worker_id(),
            &self.server.url,
            &self.server.token,
        );
        let child = command
            .spawn()
            .with_context(|| format!("spawn local worker from {}", self.worker_binary.display()))?;
        let pid = child.id().context("spawned local worker has no PID")?;
        self.child = Some(child);
        Ok(pid)
    }

    pub fn worker_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub fn route(&self) -> &LocalWorkerRoute {
        &self.route
    }

    pub fn server(&self) -> &SharedNatsServer {
        &self.server
    }

    fn stop_worker(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        signal_worker_tree(&mut child);
        let _ = std::thread::Builder::new()
            .name("harnx-worker-reaper".to_string())
            .spawn(move || {
                while matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
    }
}

/// Build the frontend-managed local worker subprocess command.
#[doc(hidden)]
pub fn build_local_worker_command(
    worker_binary: &Path,
    worker_id: &str,
    nats_url: &str,
    nats_token: &str,
) -> Command {
    let mut command = Command::new(worker_binary);
    command
        .arg("--session-scope")
        .arg(LOCAL_CLUSTER_KEY)
        .arg("--worker-id")
        .arg(worker_id)
        .arg("--manage-servers")
        .env(HARNX_NATS_URL_ENV, nats_url)
        .env(HARNX_NATS_TOKEN_ENV, nats_token)
        .stdin(Stdio::null())
        .stdout(harnx_core::logging::child_output_sink())
        .stderr(harnx_core::logging::child_output_sink())
        .kill_on_drop(true);
    configure_worker_process(&mut command);
    command
}

impl Drop for LocalWorkerSupervisor {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

fn signal_worker_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: kill is async-signal-safe and the PID came from Child.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        }
    }
    let _ = child.start_kill();
}

fn emit_worker_wait_notice(waited: Duration, worker_pid: u32) {
    let message = worker_wait_notice(waited, worker_pid);
    emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning(message.clone())));
    log::info!("{message}");
}

fn worker_wait_notice(waited: Duration, worker_pid: u32) -> String {
    format!(
        "Still waiting for local worker pid {worker_pid} to start ({}s). Ctrl-C to cancel; worker output goes to {}.",
        waited.as_secs(),
        harnx_core::logging::child_output_destination(),
    )
}

/// Last [`WORKER_OUTPUT_TAIL_BYTES`] of the log the worker writes into, for
/// error messages.
///
/// When the frontend logs to a file, anything the worker writes outside the
/// `log` facade — a panic, a `main` returning `Err`, a child process's own
/// stderr — is otherwise easy to miss while the frontend waits for readiness.
/// Empty when the worker inherits our streams instead: the output is already
/// wherever the operator is looking.
fn worker_output_tail() -> String {
    let Some(path) = harnx_core::logging::log_file_path() else {
        return String::new();
    };
    let render = |body: String| {
        if body.trim().is_empty() {
            format!("(no output in {})", path.display())
        } else {
            format!("--- tail of {} ---\n{}", path.display(), body.trim_end())
        }
    };
    let Ok(mut file) = File::open(path) else {
        return render(String::new());
    };
    let Ok(length) = file.metadata().map(|meta| meta.len()) else {
        return render(String::new());
    };
    let start = length.saturating_sub(WORKER_OUTPUT_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return render(String::new());
    }
    let mut body = Vec::new();
    let _ = file.read_to_end(&mut body);
    render(String::from_utf8_lossy(&body).into_owned())
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

    #[test]
    fn worker_bin_override_wins() {
        harnx_core::require_nextest();
        let file = tempfile::NamedTempFile::new().expect("create fake worker binary");
        unsafe { std::env::set_var(HARNX_WORKER_BIN_ENV, file.path()) };
        assert_eq!(resolve_worker_binary().unwrap(), file.path());
    }

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

    #[test]
    fn local_routes_are_nats_safe_and_unique() {
        let first = LocalWorkerRoute::new();
        let second = LocalWorkerRoute::new();
        validate_worker_id(first.worker_id()).expect("first worker id");
        validate_worker_id(second.worker_id()).expect("second worker id");
        assert_ne!(first, second);
    }

    #[test]
    fn slow_start_notice_identifies_worker_pid() {
        let notice = worker_wait_notice(Duration::from_secs(15), 42);
        assert!(
            notice.contains("worker pid 42"),
            "unexpected notice: {notice}"
        );
        assert!(notice.contains("(15s)"), "unexpected notice: {notice}");
    }

    #[tokio::test]
    async fn remote_clusters_use_shared_activation_without_starting_a_worker() {
        let supervisor = tokio::sync::Mutex::new(None);
        let route = activation_route_for_cluster(
            "prod",
            &supervisor,
            harnx_core::abort::create_abort_signal(),
        )
        .await
        .expect("resolve remote activation route");

        assert_eq!(route, SessionActivationRoute::ClusterShared);
        assert!(supervisor.lock().await.is_none());
    }
}
