use super::*;
use harnx_core::child_process::ChildProcessManager;
use std::process::{Child, Command, Stdio};

const HELPER: &str = "nats_local_server::patient::handoff::nats_owner_process";
const HELPER_ROOT: &str = "HARNX_TEST_NATS_OWNER_ROOT";

struct OtherProcess {
    child: Child,
    stop: PathBuf,
    log: PathBuf,
    _parent: ChildProcessManager,
}

impl OtherProcess {
    fn start(gate: &Gate) -> Result<Self> {
        let manager = ChildProcessManager::new();
        let log = gate.file("helper-result");
        let output = std::fs::File::create(&log)?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--ignored", "--exact", HELPER, "--nocapture"])
            .env(HELPER_ROOT, gate.fixture.path())
            .stdout(output.try_clone()?)
            .stderr(output)
            .stdin(Stdio::null());
        Ok(Self {
            child: manager.spawn_std(command)?,
            stop: gate.file("stop-helper"),
            log,
            _parent: manager,
        })
    }

    // Block this test's current-thread runtime so its supervisor cannot see
    // any unlocked interval between the two owners. The helper has its own
    // runtime and must have acquired the OS lock before it starts the wrapper.
    fn wait_for_attempt(&mut self, gate: &Gate, attempt: usize) -> Result<u32> {
        let until = std::time::Instant::now() + BACKSTOP;
        while std::time::Instant::now() < until {
            if let Some(pid) = gate.attempts().get(attempt) {
                return Ok(*pid);
            }
            if self.child.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(POLL);
        }
        bail!(
            "helper did not acquire the lock: {}",
            std::fs::read_to_string(&self.log)?
        );
    }

    async fn stop(mut self) -> Result<()> {
        std::fs::write(&self.stop, [])?;
        within(async {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    let log = std::fs::read_to_string(&self.log)?;
                    assert!(status.success() && log.contains("1 passed"), "{log}");
                    return Ok(());
                }
                sleep(POLL).await;
            }
        })
        .await
    }
}

impl Drop for OtherProcess {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.stop, []);
        let until = std::time::Instant::now() + Duration::from_secs(6);
        while std::time::Instant::now() < until {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(POLL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
#[ignore = "subprocess entry point; run by the parent test"]
async fn nats_owner_process() -> Result<()> {
    let path = PathBuf::from(std::env::var_os(HELPER_ROOT).context("missing test owner fixture")?);
    let stop = path.join("stop-helper");
    tokio::select! {
        result = own_broker(&path) => result,
        () = wait_for_file(&stop) => Ok(()),
    }
}

async fn own_broker(root: &Path) -> Result<()> {
    let server = ensure_shared_server().await?;
    assert!(server.is_owner());
    std::fs::write(root.join("helper-ready"), &server.nonce)?;
    std::future::pending().await
}

async fn wait_for_file(path: &Path) {
    while !path.exists() {
        sleep(POLL).await;
    }
}

#[tokio::test]
async fn fresh_nats_connect_waits_across_hidden_interprocess_lock_handoff() -> Result<()> {
    let Some(gate) = Gate::new()? else {
        return Ok(());
    };
    let (mut scope, mut notices) = scope()?;
    let config = frontend_config();
    gate.allow()?;
    let owner = within(ensure_shared_server()).await?;
    let client = within(config.nats_client(LOCAL_CLUSTER_KEY)).await?;
    let old = std::fs::read(nats_runtime_ports_file())?;
    // No await between this unlock, the replacement taking the lock, and the
    // fresh caller reading the generation already probed by the supervisor.
    gate.close()?;
    drop(owner);
    let mut replacement = OtherProcess::start(&gate)?;
    let pid = replacement.wait_for_attempt(&gate, 1)?;
    assert_eq!(std::fs::read(nats_runtime_ports_file())?, old);
    let fresh = config.nats_client(LOCAL_CLUSTER_KEY);
    let abort = create_abort_signal();
    let cancelled = config.nats_client_with_abort(LOCAL_CLUSTER_KEY, Some(&abort));
    tokio::pin!(fresh, cancelled);
    assert!(poll!(&mut fresh).is_pending());
    assert!(poll!(&mut cancelled).is_pending());
    expect_handoff_wait(&mut fresh, &mut cancelled, &mut notices).await?;
    abort.set_ctrlc();
    assert!(within(cancelled)
        .await
        .unwrap_err()
        .to_string()
        .contains("cancelled"));
    assert_eq!(std::fs::read(nats_runtime_ports_file())?, old);
    gate.allow()?;
    let recovered = within(fresh).await?;
    within(async {
        client
            .flush()
            .await
            .context("original client did not reconnect")
    })
    .await?;
    assert_eq!(
        client.server_info().server_id,
        recovered.server_info().server_id
    );
    assert_eq!(gate.attempts().last(), Some(&pid));
    assert_eq!(gate.attempts().len(), 2);
    scope.shutdown().await?;
    assert_eq!(
        unsafe { libc::kill(pid as libc::pid_t, 0) },
        0,
        "joiner shutdown stopped another owner"
    );
    // A client can connect on the sticky port before the owner publishes new
    // metadata. The helper writes this marker only after publication.
    within(async {
        wait_for_file(&gate.file("helper-ready")).await;
        Ok(())
    })
    .await?;
    assert_ne!(std::fs::read(nats_runtime_ports_file())?, old);
    replacement.stop().await
}

async fn expect_handoff_wait<A: Future, B: Future>(
    fresh: A,
    cancelled: B,
    notices: &mut mpsc::UnboundedReceiver<BrokerWaitNotice>,
) -> Result<()> {
    let notice_wait = async {
        let start = Instant::now();
        assert!(
            next_notice(notices).await?.contains("another owner"),
            "failed connect should invalidate cached readiness"
        );
        tokio::time::sleep_until(start + OLD_STARTUP_CUTOFF + Duration::from_secs(1)).await;
        Ok(())
    };
    tokio::select! {
        biased;
        _ = fresh => bail!("fresh caller returned without current authenticated readiness"),
        _ = cancelled => bail!("cancellable caller ended before cancellation"),
        result = notice_wait => result,
    }
}

#[tokio::test]
async fn nats_shutdown_joins_initial_startup_before_handing_off_lock() -> Result<()> {
    let Some(gate) = Gate::new()? else {
        return Ok(());
    };
    let (scope, _) = scope()?;
    check_shutdown(scope, &gate, None).await
}

#[tokio::test]
async fn nats_shutdown_joins_recovery_without_publishing_late_readiness() -> Result<()> {
    let Some(gate) = Gate::new()? else {
        return Ok(());
    };
    let (scope, _) = scope()?;
    gate.allow()?;
    within(frontend_config().nats_client(LOCAL_CLUSTER_KEY)).await?;
    let old_pid = gate.started(0).await?;
    gate.close()?;
    assert_eq!(
        unsafe { libc::kill(old_pid as libc::pid_t, libc::SIGKILL) },
        0
    );
    check_shutdown(scope, &gate, Some(old_pid)).await
}

async fn check_shutdown(
    mut scope: LocalBrokerScope,
    gate: &Gate,
    after: Option<u32>,
) -> Result<()> {
    let config = frontend_config();
    let mut pending = config.nats_client(LOCAL_CLUSTER_KEY);
    assert!(poll!(&mut pending).is_pending());
    let attempt = usize::from(after.is_some());
    let pid = gate.started(attempt).await?;
    after.into_iter().for_each(assert_reaped);
    let old = std::fs::read(nats_runtime_ports_file()).ok();
    std::fs::write(gate.file("must-be-reaped"), format!("{pid}\n"))?;
    let mut other = OtherProcess::start(gate)?;
    scope.shutdown().await?;
    assert_reaped(pid);
    assert_eq!(
        std::fs::read(nats_runtime_ports_file()).ok(),
        old,
        "shutdown published a late endpoint"
    );
    assert!(within(pending)
        .await
        .unwrap_err()
        .to_string()
        .contains("shutting down"));
    assert!(within(config.nats_client(LOCAL_CLUSTER_KEY)).await.is_err());
    let new_pid = other.wait_for_attempt(gate, attempt + 1)?;
    assert_ne!(new_pid, pid);
    assert!(
        !gate.file("ownership-overlapped").exists(),
        "lock released before reap"
    );
    gate.allow()?;
    within(async {
        wait_for_file(&gate.file("helper-ready")).await;
        Ok(())
    })
    .await?;
    other.stop().await
}
