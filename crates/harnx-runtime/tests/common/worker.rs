//! Fixtures for tests that drive a real worker daemon against a real
//! nats-server: the runtime config a local cluster needs, daemon spawning,
//! stub models, condition polling and an in-process tool server.
//!
//! Included by path from each test binary that needs it, next to that
//! binary's `mod common;` (which this module reads the nats-server handle
//! from):
//!
//! ```ignore
//! mod common;
//! #[allow(dead_code)]
//! #[path = "common/worker.rs"]
//! mod worker;
//! ```

use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_core::require_nextest;
use harnx_nats_common::{connect::NatsConnection, registry};
use harnx_runtime::config::{Config, NatsServerConfig};
use harnx_runtime::nats_lease::{
    lease_holder_in, open_lease_bucket, NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease,
};
use harnx_runtime::nats_worker::{run_worker_daemon, worker_ready_subject, WorkerDaemonConfig};
use harnx_toolset::Toolset;
use harnx_toolset_server::{
    registration_key, serve_with_client_and_identity, TOOL_REGISTRY_BUCKET,
};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::task::AbortOnDropHandle;

/// Timeout for waiting on worker/broker conditions in tests.
///
/// Generous enough to avoid flakes under CI load (contended runners, slow
/// subprocess startup). The poll loop returns immediately when the condition
/// is met, so this only matters when the system is slow.
pub const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(60);

/// The key a session's log and lease live under, for a session with no agent.
pub fn storage_key(id: &str) -> String {
    harnx_core::session_identity::session_key(None, id)
}

/// Lease timings for tests that make a worker die: short enough that waiting
/// the lease out costs seconds rather than the default half-minute, with
/// enough absolute slack that a contended runner still renews in time.
pub fn short_lease_config() -> NatsLeaseConfig {
    NatsLeaseConfig {
        ttl: Duration::from_secs(2),
        renew_interval: Duration::from_millis(500),
        replicas: 1,
        ..Default::default()
    }
}

/// Take a session's lease the way a worker does. A test that wants the worker
/// *dead* stops the returned lease's renewal rather than releasing it, so a
/// replacement has to wait the TTL out exactly as it would after a crash.
pub async fn acquire_worker_lease(
    jetstream: &async_nats::jetstream::Context,
    session_key: &str,
    worker_id: &str,
) -> Result<Arc<NatsSessionLease>> {
    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: jetstream.clone(),
        session_id: session_key,
        worker_id: worker_id.into(),
        generation: 1,
        config: short_lease_config(),
        session_metadata: None,
    })
    .await?
    .context("acquire the worker's session lease")?;
    Ok(Arc::new(lease))
}

/// Poll an async condition until it holds, bounded by [`CI_SAFE_TIMEOUT`].
pub async fn poll_until<F>(mut ready: F) -> Result<()>
where
    F: AsyncFnMut() -> Result<bool>,
{
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if ready().await? {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

/// Wait for a worker to announce itself, and fail with the daemon's own error
/// if it stopped instead of starting.
pub async fn await_worker_ready<F>(daemon: &mut F, ready: &mut async_nats::Subscriber) -> Result<()>
where
    F: std::future::Future<Output = std::result::Result<Result<()>, tokio::task::JoinError>>
        + Unpin,
{
    tokio::select! {
        announced = tokio::time::timeout(CI_SAFE_TIMEOUT, ready.next()) => {
            announced
                .context("the worker never announced itself")?
                .context("worker readiness subscription closed")?;
            Ok(())
        }
        stopped = daemon => anyhow::bail!("the worker stopped instead of starting: {stopped:?}"),
    }
}

/// Description of a single NATS server entry for test configs.
pub struct NatsServerSpec<'a> {
    pub name: &'a str,
    pub url: &'a str,
    pub token: Option<&'a str>,
}

pub fn local_nats_config(spec: NatsServerSpec<'_>) -> Config {
    let mut config = Config {
        nats_servers: vec![NatsServerConfig {
            name: spec.name.to_string(),
            url: spec.url.to_string(),
            token: spec.token.map(str::to_string),
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        }],
        ..Default::default()
    };
    config.dry_run = false;
    config
}

pub fn local_nats_runtime_config(server_url: &str) -> Arc<RwLock<Config>> {
    Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server_url,
        token: None,
    })))
}

pub async fn require_nats_server() -> Result<Option<crate::common::NatsServerHandle>> {
    require_nextest();
    let Some(server) = crate::common::spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(None);
    };
    Ok(Some(server))
}

/// Start a cluster-shared worker daemon and return once it has announced
/// itself, so a caller never publishes an activation into a broker no worker
/// is consuming from yet.
pub async fn spawn_worker_daemon_with_call_fn(
    config: Arc<RwLock<Config>>,
    worker_id: &str,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let url = config
        .read()
        .nats_servers
        .first()
        .map(|server| server.url.clone())
        .context("test worker config names a NATS server")?;
    let client = async_nats::connect(url).await?;
    let mut ready = client.subscribe(worker_ready_subject("local")).await?;
    client.flush().await?;
    let worker_config = WorkerDaemonConfig::managing("local", worker_id);
    let mut daemon =
        tokio::spawn(
            async move { run_worker_daemon(config, worker_config, Some(call_fn), None).await },
        );
    await_worker_ready(&mut daemon, &mut ready).await?;
    Ok(daemon)
}

/// Stub call_fn that increments an external counter on each LLM call, then
/// returns a final text response (single-turn).
pub fn counting_stub_call_fn(counter: Arc<AtomicUsize>) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let counter = counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok((
                "done".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

/// Wait for a condition to become true, polling every 100ms.
///
/// Uses a generous timeout (callers should pass CI_SAFE_TIMEOUT) to avoid
/// flaking under CI load. The poll loop returns immediately when the condition
/// is met, so longer timeouts only matter when the system is slow.
pub async fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if cond() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("condition not met within {timeout:?}")
}

/// Wait until no worker holds this session and the worker's own session count
/// is back to zero: the session has been fully let go of, not merely acked.
pub async fn wait_for_worker_session_cleanup(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<()> {
    let lease_config = NatsLeaseConfig::default();
    // The bucket appears when a worker first needs it, which is a moment
    // after the worker announced itself; waiting for it beats assuming it is
    // already there.
    let mut bucket = None;
    poll_until(async || {
        bucket = open_lease_bucket(jetstream, &lease_config).await;
        Ok(bucket.is_some())
    })
    .await
    .context("no worker created the lease bucket")?;
    let lease_bucket = bucket.context("lease bucket")?;
    let session_key = storage_key(session_id);
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if lease_holder_in(&lease_bucket, &lease_config, &session_key)
                .await?
                .is_none()
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    wait_until(CI_SAFE_TIMEOUT, || {
        harnx_runtime::nats_metrics::snapshot().active_sessions_per_worker == 0
    })
    .await
}

/// Set an environment variable for the duration of a test, restoring whatever
/// it held before. Nextest gives each test binary its own process, but a
/// fixture that runs more than once in one binary still has to put the
/// environment back.
pub struct EnvGuard(&'static str, Option<std::ffi::OsString>);

impl EnvGuard {
    pub fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self(key, previous)
    }

    /// The scope and connection a worker and its tool servers have to agree
    /// on, as the frontend hands them over.
    pub fn tool_server_environment(scope: &ServerScope, url: &str) -> [Self; 3] {
        [
            Self::set(HARNX_SERVER_SCOPE, scope.as_str()),
            Self::set("HARNX_NATS_URL", url),
            Self::set("HARNX_NATS_TOKEN", ""),
        ]
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => unsafe { std::env::set_var(self.0, value) },
            None => unsafe { std::env::remove_var(self.0) },
        }
    }
}

/// Serve a toolset in-process under `scope` and return once the worker can
/// discover it, so a test never races its own tool registration.
pub async fn start_tool_server<T: Toolset + 'static>(
    js: &async_nats::jetstream::Context,
    client: async_nats::Client,
    scope: ServerScope,
    tool: Arc<T>,
) -> Result<AbortOnDropHandle<Result<()>>> {
    let registry =
        registry::ensure_bucket_with_ttl(js, TOOL_REGISTRY_BUCKET, registry::REGISTRATION_TTL, 1)
            .await?;
    // An unpackaged, unconfigured server's identity token is its name behind
    // the two empty segments `server_identity_token` joins with.
    let key = registration_key(&scope, &format!("____{}", tool.name()));
    let mut registered = registry.watch(&key).await?;
    let server = AbortOnDropHandle::new(tokio::spawn(serve_with_client_and_identity(
        tool,
        scope,
        NatsConnection {
            client,
            replicas: 1,
        },
        Default::default(),
    )));
    tokio::time::timeout(CI_SAFE_TIMEOUT, registered.next())
        .await?
        .context("registration watch closed")??;
    Ok(server)
}
