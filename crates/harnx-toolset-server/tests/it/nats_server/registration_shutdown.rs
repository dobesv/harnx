//! Registration shutdown policies, shared-key retention, and abandoned-key expiry.

use super::*;

// ============================================================================
// HA registration-shutdown policy tests
// ============================================================================

/// When `RegistrationShutdown::Expire` is set, shutdown must NOT delete the
/// registration key. This is the HA gateway behavior: multiple replicas share
/// one key, and a replica shutting down must not remove it for the survivors.
#[tokio::test(flavor = "multi_thread")]
async fn expire_policy_skips_registration_deletion_on_shutdown() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    let instance_id = harnx_core::instance::ServerScope::new();
    let shutdown = CancellationToken::new();
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;

    let server_client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let server_instance_id = instance_id.clone();
    let server_shutdown = shutdown.clone();
    let key = registration_key(&instance_id, "____test");
    let server_task = tokio::spawn(async move {
        serve_with_shutdown(
            Arc::new(TestToolset::default()),
            server_instance_id,
            NatsConnection {
                client: server_client,
                replicas: 1,
            },
            ServeLifecycle::new(server_shutdown, None)
                .with_registration_shutdown(RegistrationShutdown::Expire),
        )
        .await
    });

    // Wait for registration to appear (bucket is created lazily by serve_with_shutdown)
    let registry = wait_for_registry_bucket(&client).await?;

    // Wait for key to appear
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if registry.get(&key).await?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for registration to appear");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Trigger graceful shutdown
    shutdown.cancel();
    server_task
        .await
        .context("join server task")?
        .context("server exited with error")?;

    // Key must STILL be present (TTL will reap it later, not shutdown)
    assert!(
        registry.get(&key).await?.is_some(),
        "registration must NOT be deleted on shutdown when using Expire policy"
    );
    Ok(())
}

/// Verify that the default `Expire` policy does NOT delete the registration
/// on shutdown, and that explicit `DeleteIfCurrent` policy still works.
#[tokio::test(flavor = "multi_thread")]
async fn default_expire_policy_retains_registration() -> Result<()> {
    harnx_core::require_nextest();
    let Some(mut harness) = TestHarness::start().await? else {
        return Ok(());
    };
    // Wait for registration to appear (bucket is created lazily)
    let registry = wait_for_registry_bucket(&harness.client).await?;
    let key = registration_key(&harness.instance_id, "____test");

    // Wait for key to appear
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if registry.get(&key).await?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for registration to appear");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    harness.shutdown().await;

    // Default policy is Expire, so key must still be present
    assert!(
        registry.get(&key).await?.is_some(),
        "registration must NOT be deleted on shutdown with default Expire policy"
    );
    Ok(())
}

/// Verify that the explicit `DeleteIfCurrent` policy still deletes the
/// registration on shutdown. Singleton tool servers may opt into this
/// behavior when they are guaranteed never to share their identity.
#[tokio::test(flavor = "multi_thread")]
async fn explicit_delete_if_current_policy_removes_registration_on_shutdown() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(());
    };

    let instance_id = harnx_core::instance::ServerScope::new();
    let shutdown = CancellationToken::new();
    let toolset = TestToolset::default();
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let readiness = harnx_healthz::Readiness::default();

    // Explicitly opt into DeleteIfCurrent
    let server_task = tokio::spawn({
        let instance_id = instance_id.clone();
        let shutdown = shutdown.clone();
        async move {
            serve_with_shutdown(
                Arc::new(toolset),
                instance_id,
                NatsConnection {
                    client,
                    replicas: 1,
                },
                ServeLifecycle::new(shutdown, Some(readiness))
                    .with_registration_shutdown(RegistrationShutdown::DeleteIfCurrent),
            )
            .await
        }
    });

    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;

    // Wait for registration to appear
    let registry = wait_for_registry_bucket(&client).await?;
    let key = registration_key(&instance_id, "____test");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if registry.get(&key).await?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for registration to appear");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server_task)
        .await
        .context("explicit DeleteIfCurrent server shutdown timed out")?
        .context("join explicit DeleteIfCurrent server")??;

    // Explicit DeleteIfCurrent policy must delete the key
    assert!(
        registry.get(&key).await?.is_none(),
        "registration must be deleted on shutdown with explicit DeleteIfCurrent policy"
    );
    Ok(())
}

// ============================================================================
// TTL expiry tests
// ============================================================================

/// Exercise real server shutdown, then let its abandoned registration expire.
#[tokio::test(flavor = "multi_thread")]
async fn abandoned_registration_expires_by_ttl() -> Result<()> {
    harnx_core::require_nextest();
    let Some(mut harness) = TestHarness::start().await? else {
        return Ok(());
    };
    let registry = wait_for_registry_bucket(&harness.client).await?;
    let key = registration_key(&harness.instance_id, "____test");
    let revision = wait_for_revision_beyond(&harness.client, &key, 0).await?;
    let registration = wait_for_registration(&harness.client, &harness.instance_id).await?;
    assert_eq!(registration.server, "test");

    harness.shutdown.cancel();
    tokio::time::timeout(
        Duration::from_secs(10),
        harness.server_task.take().context("running server task")?,
    )
    .await
    .context("server shutdown timed out")?
    .context("join server task")??;
    assert!(
        registry.get(&key).await?.is_some(),
        "server shutdown must retain its registration until TTL expiry"
    );
    assert_eq!(
        registration_revision(&harness.client, &key).await?,
        Some(revision)
    );

    // Shorten only this test broker's bucket after shutdown: startup reconciles
    // it to the production TTL. No synthetic registration or publisher remains.
    let mut config = registry.status().await?.info.config;
    assert_eq!(
        config.max_age,
        harnx_nats_common::registry::REGISTRATION_TTL
    );
    config.max_age = Duration::from_secs(1);
    config.duplicate_window = config.duplicate_window.min(config.max_age);
    async_nats::jetstream::new(harness.client.clone())
        .update_stream(config)
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.get(&key).await?.is_some() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("abandoned server registration did not expire by bucket TTL")??;
    assert!(
        registry.entry(&key).await?.is_none(),
        "TTL must remove the registration, not leave a deletion marker"
    );
    Ok(())
}

// ============================================================================
// Latest-writer shared-key retention tests
// ============================================================================

async fn spawn_departing_replica(
    survivor: &TestHarness,
    refresh_interval: Duration,
) -> Result<(CancellationToken, tokio::task::JoinHandle<Result<()>>)> {
    let shutdown_b = CancellationToken::new();
    let client_b = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&survivor._server.url)
        .await?;
    let server_b_task = tokio::spawn({
        let instance_id = survivor.instance_id.clone();
        let shutdown_b = shutdown_b.clone();
        async move {
            serve_with_shutdown(
                Arc::new(TestToolset::default()),
                instance_id,
                NatsConnection {
                    client: client_b,
                    replicas: 1,
                },
                ServeLifecycle::new(shutdown_b, None).with_refresh_interval(refresh_interval),
            )
            .await
        }
    });
    Ok((shutdown_b, server_b_task))
}

/// The departing replica must own the latest revision; otherwise revision CAS
/// can hide the old shutdown-deletion bug. Keep refresh outside the test window
/// and check the exact revision so a survivor cannot mask deletion by republishing.
#[tokio::test(flavor = "multi_thread")]
async fn default_shutdown_retains_shared_key_when_latest_writer_stops() -> Result<()> {
    harnx_core::require_nextest();
    let refresh_interval = Duration::from_secs(3600);
    let started = Instant::now();
    let Some(mut survivor) =
        TestHarness::with_toolset(TestToolset::default(), Some(refresh_interval)).await?
    else {
        return Ok(());
    };
    let key = registration_key(&survivor.instance_id, "____test");
    let survivor_revision = wait_for_revision_beyond(&survivor.client, &key, 0).await?;
    let registry = wait_for_registry_bucket(&survivor.client).await?;

    // Only B can advance the revision: A has registered and cannot refresh yet.
    let (shutdown_b, server_b_task) = spawn_departing_replica(&survivor, refresh_interval).await?;
    let departing_revision =
        wait_for_revision_beyond(&survivor.client, &key, survivor_revision).await?;
    assert!(departing_revision > survivor_revision);

    shutdown_b.cancel();
    tokio::time::timeout(Duration::from_secs(10), server_b_task)
        .await
        .context("latest writer B shutdown timed out")?
        .context("join latest writer B")??;
    assert!(
        registry.get(&key).await?.is_some(),
        "latest writer B shutdown must not delete the shared registration"
    );
    assert_eq!(
        registration_revision(&survivor.client, &key).await?,
        Some(departing_revision),
        "the retained revision must be B's, not a refresh masking deletion"
    );
    assert!(
        started.elapsed() < refresh_interval,
        "refresh must not run during the test"
    );

    let request = make_test_tool_request("echo", "post-b-shutdown", json!({ "value": 42 }));
    let reply = tokio::time::timeout(Duration::from_secs(10), survivor.call_tool(&request))
        .await
        .context("request to survivor A timed out after B stopped")??;
    assert_eq!(
        reply.result.expect("survivor A must still serve"),
        request.args
    );
    assert_eq!(survivor.toolset.echo_invocations.load(Ordering::SeqCst), 1);

    survivor.shutdown.cancel();
    tokio::time::timeout(
        Duration::from_secs(10),
        survivor
            .server_task
            .take()
            .context("running survivor task")?,
    )
    .await
    .context("survivor A shutdown timed out")?
    .context("join survivor A")??;
    assert!(
        registry.get(&key).await?.is_some(),
        "last replica shutdown must leave registration for TTL expiry"
    );
    assert_eq!(
        registration_revision(&survivor.client, &key).await?,
        Some(departing_revision)
    );
    Ok(())
}
