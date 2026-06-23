//! Integration tests for the per-session NATS KV lease (P2.1).
//! Spawns a real local nats-server (see tests/common). Multi-thread runtime
//! is required for async-nats + block-style waits.

mod common;

use anyhow::{Context, Result};
use common::spawn_nats_server;
use harnx_core::require_nextest;
use harnx_runtime::nats_lease::{NatsLeaseConfig, NatsSessionLease};
use std::time::{Duration, Instant};
use tokio::time::sleep;

fn fast_lease_config() -> NatsLeaseConfig {
    NatsLeaseConfig {
        ttl: Duration::from_secs(2),
        renew_interval: Duration::from_millis(400),
        replicas: 1,
        tombstone_ttl: Duration::from_secs(10),
        ..Default::default()
    }
}

async fn jetstream(url: &str) -> Result<async_nats::jetstream::Context> {
    Ok(async_nats::jetstream::new(async_nats::connect(url).await?))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_contention_allows_exactly_one_holder() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };
    let js = jetstream(server.url()).await?;
    let cfg = fast_lease_config();

    let one = NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "contention",
        worker_id: "worker-a".to_string(),
        generation: 1,
        config: cfg.clone(),
    })
    .await?;
    let two = NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "contention",
        worker_id: "worker-b".to_string(),
        generation: 1,
        config: cfg.clone(),
    })
    .await?;

    assert!(
        one.is_some() ^ two.is_some(),
        "exactly one worker must hold the lease (one={}, two={})",
        one.is_some(),
        two.is_some()
    );
    if let Some(lease) = one.or(two) {
        assert!(lease.is_held());
        assert!(lease.fence_token() > 0);
        lease.release().await.context("release should succeed")?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_release_succeeds_after_renewals() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };
    let js = jetstream(server.url()).await?;
    let cfg = fast_lease_config();

    let lease = NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "release",
        worker_id: "worker-a".to_string(),
        generation: 1,
        config: cfg,
    })
    .await?
    .context("acquire")?;
    let initial_fence = lease.fence_token();
    // Let at least two renewal ticks bump the fence.
    sleep(Duration::from_millis(1000)).await;
    assert!(lease.is_held(), "lease should still be held after renewals");
    assert!(
        lease.fence_token() > initial_fence,
        "fence should advance on renewal (initial={}, now={})",
        initial_fence,
        lease.fence_token()
    );
    // Release must succeed even though the fence advanced via renewal CAS.
    lease
        .release()
        .await
        .context("release after renewals must succeed")?;

    // After release the key is gone: a new worker can acquire.
    let reacquire = NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "release",
        worker_id: "worker-b".to_string(),
        generation: 2,
        config: fast_lease_config(),
    })
    .await?;
    assert!(
        reacquire.is_some(),
        "lease should be acquirable after release"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_failover_after_holder_stops_renewing() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };
    let js = jetstream(server.url()).await?;
    let cfg = fast_lease_config();

    let lease_one = NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "failover",
        worker_id: "worker-a".to_string(),
        generation: 1,
        config: cfg.clone(),
    })
    .await?
    .context("first acquire")?;
    // Simulate worker death: stop renewing and forget the lease (no release).
    lease_one.stop_renewal_for_test().await;
    std::mem::forget(lease_one);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(lease_two) =
            NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id: "failover",
                worker_id: "worker-b".to_string(),
                generation: 1,
                config: cfg.clone(),
            })
            .await?
        {
            lease_two.release().await?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("second worker failed to acquire after TTL expiry");
        }
        sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_renewal_survives_long_operation() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };
    let js = jetstream(server.url()).await?;
    let cfg = fast_lease_config(); // ttl 2s, renew 400ms

    let lease = NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "long-op",
        worker_id: "worker-a".to_string(),
        generation: 1,
        config: cfg,
    })
    .await?
    .context("acquire")?;

    // Simulate a long (> ttl) operation on the holder's thread; the renewal
    // task runs on the multi-thread runtime and keeps the lease alive.
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_secs(4))).await?;
    assert!(
        lease.is_held(),
        "renewal task should keep the lease alive across a >TTL op"
    );
    lease.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_loss_is_signalled_on_watch() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };
    let js = jetstream(server.url()).await?;
    let cfg = fast_lease_config();

    let lease = NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "loss",
        worker_id: "worker-a".to_string(),
        generation: 1,
        config: cfg,
    })
    .await?
    .context("acquire")?;
    let mut watch = lease.lost_watch();
    assert!(*watch.borrow(), "watch starts held=true");

    // Mark the lease lost (simulates renewal failure / detected loss).
    lease.mark_lost_for_test();

    // Watch must transition to false promptly.
    let changed = tokio::time::timeout(Duration::from_secs(2), watch.changed()).await;
    assert!(changed.is_ok(), "lost-watch should fire");
    assert!(!*watch.borrow(), "lost-watch should report false");
    assert!(!lease.is_held());
    Ok(())
}
