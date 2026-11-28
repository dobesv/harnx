use super::*;
use anyhow::{bail, Context, Result};
use futures_util::poll;
use harnx_core::abort::create_abort_signal;
use harnx_core::config_paths::nats_runtime_ports_file;
use harnx_runtime::config::{Config, ConfigLock, GlobalConfig, NatsRouting, LOCAL_CLUSTER_KEY};
use harnx_runtime::nats_local_server::{BrokerWaitNotice, LocalBrokerScope};
use harnx_runtime::{NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};

#[path = "patient/handoff.rs"]
mod handoff;

#[path = "patient/routing.rs"]
mod routing;

const BACKSTOP: Duration = Duration::from_secs(60);
const OLD_STARTUP_CUTOFF: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(25);

struct Gate {
    fixture: TempDir,
    _data: DataDirGuard,
    _binary: crate::worker::EnvGuard,
}

impl Gate {
    fn new() -> Result<Option<Self>> {
        require_nextest();
        let Some(real) = nats_server_binary() else {
            eprintln!("skipping NATS integration test: nats-server binary not found");
            return Ok(None);
        };
        let fixture = tempfile::tempdir()?;
        let root = fixture.path();
        let wrapper = root.join("nats-server");
        std::fs::create_dir(root.join("real"))?;
        std::os::unix::fs::symlink(real.canonicalize()?, root.join("real/nats-server"))?;
        std::fs::write(&wrapper, WRAPPER)?;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
        let data = DataDirGuard::isolated(&root.join("data"));
        let binary = crate::worker::EnvGuard::set(
            "NATS_SERVER_BIN",
            wrapper.to_str().context("wrapper path")?,
        );
        Ok(Some(Self {
            fixture,
            _data: data,
            _binary: binary,
        }))
    }

    fn file(&self, name: &str) -> PathBuf {
        self.fixture.path().join(name)
    }

    fn allow(&self) -> Result<()> {
        std::fs::write(self.file("go"), [])?;
        Ok(())
    }

    fn close(&self) -> Result<()> {
        std::fs::remove_file(self.file("go"))?;
        Ok(())
    }

    fn attempts(&self) -> Vec<u32> {
        std::fs::read_to_string(self.file("starts"))
            .unwrap_or_default()
            .lines()
            .map(|pid| pid.parse().expect("wrapper PID"))
            .collect()
    }

    async fn started(&self, attempt: usize) -> Result<u32> {
        timeout(BACKSTOP, async {
            loop {
                if let Some(pid) = self.attempts().get(attempt) {
                    return *pid;
                }
                sleep(POLL).await;
            }
        })
        .await
        .context("wrapper never started")
    }
}

// Preserve the wrapper's PID with exec, and let real NATS publish its own
// .ports file. The script does not bind a port or synthesize readiness.
const WRAPPER: &str = r#"#!/bin/sh
set -eu
root=${0%/*}
if [ "$1" = --version ]; then exec "$root/real/nats-server" "$@"; fi
printf '%s\n' "$$" >> "$root/starts"
if [ -f "$root/fail" ]; then exit 23; fi
if [ -f "$root/must-be-reaped" ]; then
  read -r previous_pid < "$root/must-be-reaped"
  if kill -0 "$previous_pid" 2>/dev/null; then
    printf '%s\n' "$previous_pid" > "$root/ownership-overlapped"
    exit 24
  fi
fi
trap 'exit 0' TERM
while [ ! -f "$root/go" ]; do sleep 0.025; done
exec "$root/real/nats-server" "$@"
"#;

fn frontend_config() -> Config {
    use harnx_runtime::config::{Agent, AgentConfig};

    let mut config = Config::default();
    let mut agent = AgentConfig::from_prompt("");
    agent.set_name("patient-broker-test");
    config.agent = Some(Agent::new(agent));
    config.nats_routing = NatsRouting::FrontendLocal;
    config
}

fn scope() -> Result<(LocalBrokerScope, mpsc::UnboundedReceiver<BrokerWaitNotice>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let scope = LocalBrokerScope::begin(Arc::new(move |notice| {
        let _ = tx.send(notice);
    }))?;
    Ok((scope, rx))
}

async fn within<T>(wait: impl Future<Output = Result<T>>) -> Result<T> {
    timeout(BACKSTOP, wait).await.context("test wait expired")?
}

fn assert_reaped(pid: u32) {
    // The child is ours; kill(0) also sees a zombie that wasn't reaped.
    assert_eq!(
        unsafe { libc::kill(pid as libc::pid_t, 0) },
        -1,
        "PID {pid} is still present"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

async fn next_notice(notices: &mut mpsc::UnboundedReceiver<BrokerWaitNotice>) -> Result<String> {
    within(async {
        notices
            .recv()
            .await
            .context("notice route closed")
            .map(|notice| notice.to_string())
    })
    .await
}

fn global() -> GlobalConfig {
    Arc::new(ConfigLock::new(frontend_config()))
}

fn session_config() -> NatsSessionConfig {
    NatsSessionConfig {
        cluster: LOCAL_CLUSTER_KEY.into(),
        initializer: SessionInitializer::named("patient-broker-test", Default::default()),
        session_id: None,
        activation_route: SessionActivationRoute::ClusterShared,
    }
}

#[tokio::test]
async fn real_nats_waits_past_old_port_cutoff_and_reserves_on_original_pid() -> Result<()> {
    let Some(gate) = Gate::new()? else {
        return Ok(());
    };
    let (mut scope, mut notices) = scope()?;
    let config = global();
    let reserve = Config::reserve_new_session_id(&config);
    let listing_config = config.read().clone();
    let listing = listing_config.list_remote_sessions_with_meta(LOCAL_CLUSTER_KEY);
    tokio::pin!(reserve, listing);
    assert!(poll!(&mut reserve).is_pending());
    assert!(poll!(&mut listing).is_pending());
    let pid = gate.started(0).await?;
    let joiner = ensure_shared_server();
    tokio::pin!(joiner);
    assert!(poll!(&mut joiner).is_pending());
    let cutoff = Instant::now() + OLD_STARTUP_CUTOFF + Duration::from_secs(1);
    let abort = create_abort_signal();
    let session = NatsSession::from_global_config(session_config(), &config, abort.clone());
    tokio::pin!(session);
    assert!(poll!(&mut session).is_pending());
    abort.set_ctrlc();
    let error = within(session)
        .await
        .err()
        .context("cancelled waiter returned ready")?;
    assert!(format!("{error:#}").contains("cancelled while waiting for NATS"));
    keep_gate_closed(&mut reserve, &mut joiner, &mut listing, cutoff).await?;
    assert_eq!(gate.attempts(), [pid], "owner must not respawn");
    assert!(
        !nats_runtime_ports_file().exists(),
        "gate delayed actual publication"
    );
    let first = next_notice(&mut notices).await?;
    assert!(first.contains(&format!("PID {pid}")) && first.contains("report its port"));
    assert!(next_notice(&mut notices)
        .await?
        .contains(&format!("PID {pid}")));
    assert!(
        notices.try_recv().is_err(),
        "multiple callers must not multiply notices"
    );
    gate.allow()?;
    let (session_id, joined) = within(async { tokio::try_join!(reserve, joiner) }).await?;
    assert_eq!(session_id.len(), 6);
    within(listing).await?;
    assert!(!joined.is_owner());
    assert_eq!(
        gate.attempts(),
        [pid],
        "success must come from the gated PID"
    );
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, 0);
    scope.shutdown().await?;
    assert_reaped(pid);
    Ok(())
}

async fn keep_gate_closed(
    owner: impl Future,
    joiner: impl Future,
    listing: impl Future,
    cutoff: Instant,
) -> Result<()> {
    tokio::select! {
        biased;
        _ = owner => bail!("reservation spent its operation budget on broker startup"),
        _ = joiner => bail!("joiner stopped waiting while the owner was still live"),
        _ = listing => bail!("listing spent its operation budget on broker startup"),
        () = tokio::time::sleep_until(cutoff) => Ok(()),
    }
}

#[tokio::test]
async fn cancelled_waiter_does_not_poison_later_nats_recovery() -> Result<()> {
    let Some(gate) = Gate::new()? else {
        return Ok(());
    };
    let (mut scope, _) = scope()?;
    let config = frontend_config();
    let abort = create_abort_signal();
    let cancelled = config.nats_client_with_abort(LOCAL_CLUSTER_KEY, Some(&abort));
    let mut survivor = config.nats_client(LOCAL_CLUSTER_KEY);
    tokio::pin!(cancelled);
    assert!(poll!(&mut cancelled).is_pending());
    assert!(poll!(&mut survivor).is_pending());
    let first_pid = gate.started(0).await?;
    abort.set_ctrlc();
    assert!(within(cancelled)
        .await
        .unwrap_err()
        .to_string()
        .contains("cancelled"));
    gate.allow()?;
    let existing = within(survivor).await?;
    let old_info = existing.server_info();
    assert_eq!(gate.attempts(), [first_pid]);
    let old_metadata = std::fs::read(nats_runtime_ports_file())?;
    gate.close()?;
    // Simulate broker death, leaving its owner frontend and the original
    // client alive. The cancelled waiter must not poison idle recovery.
    assert_eq!(
        unsafe { libc::kill(first_pid as libc::pid_t, libc::SIGKILL) },
        0
    );
    let replacement = gate.started(1).await?;
    assert_reaped(first_pid);
    assert_ne!(replacement, first_pid);
    assert_eq!(std::fs::read(nats_runtime_ports_file())?, old_metadata);
    let fresh = config.nats_client(LOCAL_CLUSTER_KEY);
    let during_recovery = create_abort_signal();
    let cancelled = config.nats_client_with_abort(LOCAL_CLUSTER_KEY, Some(&during_recovery));
    tokio::pin!(fresh, cancelled);
    assert!(poll!(&mut fresh).is_pending());
    assert!(poll!(&mut cancelled).is_pending());
    during_recovery.set_ctrlc();
    assert!(within(cancelled).await.is_err());
    gate.allow()?;
    let new_client = within(fresh).await?;
    assert_ne!(new_client.server_info().server_id, old_info.server_id);
    assert_eq!(new_client.server_info().port, old_info.port);
    within(async {
        existing
            .flush()
            .await
            .context("existing client did not reconnect")
    })
    .await?;
    assert_eq!(
        existing.server_info().server_id,
        new_client.server_info().server_id
    );
    assert_eq!(gate.attempts(), [first_pid, replacement]);
    scope.shutdown().await?;
    assert_reaped(replacement);
    Ok(())
}

#[tokio::test]
async fn genuine_nats_child_exit_exhausts_attempts_and_next_acquisition_can_retry() -> Result<()> {
    let Some(gate) = Gate::new()? else {
        return Ok(());
    };
    let (mut scope, _) = scope()?;
    let config = frontend_config();
    std::fs::write(gate.file("fail"), [])?;
    let failed = within(config.nats_client(LOCAL_CLUSTER_KEY))
        .await
        .unwrap_err();
    let diagnostic = format!("{failed:#}");
    assert!(
        diagnostic.contains("after 5 attempts") && diagnostic.contains("exit status: 23"),
        "{diagnostic}"
    );
    let attempts = gate.attempts();
    assert_eq!(attempts.len(), 5);
    attempts.into_iter().for_each(assert_reaped);
    std::fs::remove_file(gate.file("fail"))?;
    gate.allow()?;
    within(config.nats_client(LOCAL_CLUSTER_KEY)).await?;
    assert_eq!(gate.attempts().len(), 6);
    scope.shutdown().await
}
