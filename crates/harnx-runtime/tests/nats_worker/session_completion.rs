//! Broker-backed activation and completion barriers for worker integration tests.
use super::*;

pub(super) async fn local_test_nats(server_url: &str) -> Result<async_nats::jetstream::Context> {
    Ok(async_nats::jetstream::new(
        async_nats::connect(server_url).await?,
    ))
}

pub(super) async fn activate_session(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<()> {
    let store = SessionMetadataStore::ensure(jetstream, 1).await?;
    if store.get(session_id).await?.is_none() {
        seed_session_metadata(jetstream, session_id).await?;
    }
    publish_session_activate(jetstream, "local", &SessionActivate::new(session_id)).await?;
    Ok(())
}

pub(super) async fn wait_for_worker_daemon_idle(
    js: &async_nats::jetstream::Context,
    session_id: &str,
    metrics_before_lease_acquisitions: u64,
) -> Result<()> {
    wait_until(CI_SAFE_TIMEOUT, || {
        harnx_runtime::nats_metrics::snapshot().lease_acquisitions
            > metrics_before_lease_acquisitions
    })
    .await?;
    // Lease acquisition precedes service preparation and active-session
    // accounting. A zero active count alone can still mean "not started yet".
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if !harnx_runtime::nats_lease::session_has_active_lease(js, session_id).await?
                && harnx_runtime::nats_metrics::snapshot().active_sessions_per_worker == 0
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_wait_includes_activation_preparation() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = local_test_nats(server.url()).await?;
    let before = harnx_runtime::nats_metrics::snapshot().lease_acquisitions;
    // Activation owns the lease while preparing services, before it increments
    // active_sessions_per_worker. Hold that exact state without scheduling races.
    let lease = harnx_runtime::nats_lease::NatsSessionLease::acquire(
        harnx_runtime::nats_lease::NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: "preparing-session",
            worker_id: "preparing-worker".into(),
            generation: 1,
            config: Default::default(),
            session_metadata: None,
        },
    )
    .await?
    .expect("test lease");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_worker_daemon_idle(&js, "preparing-session", before),
        )
        .await
        .is_err(),
        "a preparing activation is not a finished turn"
    );
    lease.release().await?;
    wait_for_worker_daemon_idle(&js, "preparing-session", before).await?;
    Ok(())
}
