use super::*;
use tokio::sync::oneshot;

pub(super) async fn assert_echo_hook_works(
    client: &async_nats::Client,
    scope: &ServerScope,
) -> Result<()> {
    let payload = HookPayload {
        session_id: "survivor-session".to_string(),
        cwd: std::env::current_dir()?,
        resume_count: 0,
        hook_event: HookEvent::PreToolUse {
            tool_name: "example".to_string(),
            tool_input: json!({"input": true}),
            tool_use_id: "survivor-call".to_string(),
        },
    };
    let message = client
        .send_request(
            scope.hook_subject("echo", "PreToolUse"),
            async_nats::Request::new()
                .headers(harnx_hookset_server::hook_request_headers(
                    "survivor-session",
                    "survivor-call",
                ))
                .payload(serde_json::to_vec(&payload)?.into())
                .timeout(Some(Duration::from_secs(10))),
        )
        .await
        .context("surviving hook server must still reply")?;
    let outcome = harnx_hookset_server::decode_hook_reply(&message.payload)?;
    assert_eq!(outcome.control, HookResultControl::Continue);
    assert_eq!(
        outcome.result.mutated_tool_input,
        Some(json!({"seen": true}))
    );
    Ok(())
}

struct PausedSurvivor {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<()>>,
    resume: oneshot::Sender<()>,
    revision: u64,
}

/// Complete the pause/flush handshake before another replica can publish.
async fn spawn_paused_survivor(
    url: &str,
    scope: &ServerScope,
    client: &async_nats::Client,
) -> Result<PausedSurvivor> {
    let key = hook_registration_key(scope, "echo");
    let shutdown_a = CancellationToken::new();
    let (pause, pause_rx) = oneshot::channel();
    let (paused, paused_rx) = oneshot::channel();
    let (resume, resume_rx) = oneshot::channel();
    let server_a = tokio::spawn({
        let scope = scope.clone();
        let shutdown = shutdown_a.clone();
        let connection = NatsConnection {
            client: connect_test_client(url).await?,
            replicas: 1,
        };
        async move {
            let pause_client = connection.client.clone();
            let serving = serve_with_shutdown(
                Arc::new(EchoHook),
                scope,
                connection,
                ServeLifecycle::new(shutdown, None),
            );
            tokio::pin!(serving);
            tokio::select! {
                biased;
                _ = pause_rx => {},
                result = &mut serving => return result,
            }
            // Fence any publish already queued before suspending the serve future.
            pause_client
                .flush()
                .await
                .context("flush survivor A before pause")?;
            let _ = paused.send(());
            resume_rx.await.context("resume survivor A")?;
            serving.await
        }
    });
    wait_for_revision_beyond(client, &key, 0).await?;
    pause
        .send(())
        .map_err(|_| anyhow::anyhow!("survivor A exited before pause"))?;
    tokio::time::timeout(Duration::from_secs(10), paused_rx)
        .await
        .context("survivor A did not pause")?
        .context("survivor A exited before acknowledging pause")?;
    let revision = registration_revision(client, &key)
        .await?
        .context("paused survivor A registration")?;

    Ok(PausedSurvivor {
        shutdown: shutdown_a,
        task: server_a,
        resume,
        revision,
    })
}

/// Stop polling A after its initial registration so it cannot refresh while B
/// publishes and shuts down. No production refresh override or timing race.
#[tokio::test(flavor = "multi_thread")]
async fn latest_writer_shutdown_retains_shared_registration() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let scope = ServerScope::new();
    let key = hook_registration_key(&scope, "echo");
    let client = connect_test_client(&server.url).await?;
    let survivor = spawn_paused_survivor(&server.url, &scope, &client).await?;

    let shutdown_b = CancellationToken::new();
    let server_b = spawn_echo_hook_server(&server.url, scope.clone(), shutdown_b.clone()).await?;
    let revision_b = wait_for_revision_beyond(&client, &key, survivor.revision).await?;
    assert!(revision_b > survivor.revision);
    shutdown_b.cancel();
    tokio::time::timeout(Duration::from_secs(10), server_b)
        .await
        .context("latest writer B shutdown timed out")?
        .context("join latest writer B")??;

    let registry = async_nats::jetstream::new(client.clone())
        .get_key_value(HOOK_REGISTRY_BUCKET)
        .await?;
    assert!(
        registry.get(&key).await?.is_some(),
        "latest writer B shutdown must not delete the shared hook registration"
    );
    assert_eq!(
        registration_revision(&client, &key).await?,
        Some(revision_b),
        "B's registration must survive before A is allowed to refresh"
    );

    survivor
        .resume
        .send(())
        .map_err(|_| anyhow::anyhow!("survivor A exited while paused"))?;
    assert_echo_hook_works(&client, &scope).await?;
    survivor.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), survivor.task)
        .await
        .context("survivor A shutdown timed out")?
        .context("join survivor A")??;
    assert!(
        registry.get(&key).await?.is_some(),
        "last hook replica shutdown must leave registration for TTL expiry"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn abandoned_hook_registration_expires_by_ttl() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let scope = ServerScope::new();
    let key = hook_registration_key(&scope, "echo");
    let client = connect_test_client(&server.url).await?;
    let shutdown = CancellationToken::new();
    let readiness = harnx_healthz::Readiness::default();
    let server_task = tokio::spawn(serve_with_shutdown(
        Arc::new(EchoHook),
        scope.clone(),
        NatsConnection {
            client: connect_test_client(&server.url).await?,
            replicas: 1,
        },
        ServeLifecycle::new(shutdown.clone(), Some(readiness.clone())),
    ));
    let registration = wait_for_registration(&client, &scope).await?;
    assert_eq!(registration.server, "echo");
    let revision = wait_for_revision_beyond(&client, &key, 0).await?;
    assert!(readiness.is_ready());
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server_task)
        .await
        .context("hook server shutdown timed out")?
        .context("join hook server")??;
    assert!(!readiness.is_ready(), "shutdown must clear readiness");

    let jetstream = async_nats::jetstream::new(client.clone());
    let registry = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await?;
    assert!(
        registry.get(&key).await?.is_some(),
        "hook server shutdown must retain its registration until TTL expiry"
    );
    assert_eq!(registration_revision(&client, &key).await?, Some(revision));

    // Startup reconciles this bucket to 90s. Shorten only this test broker's
    // TTL after real server shutdown, with no synthetic registration publisher.
    let mut config = registry.status().await?.info.config;
    assert_eq!(
        config.max_age,
        harnx_nats_common::registry::REGISTRATION_TTL
    );
    config.max_age = Duration::from_secs(1);
    config.duplicate_window = config.duplicate_window.min(config.max_age);
    jetstream.update_stream(config).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.get(&key).await?.is_some() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("abandoned hook registration did not expire by bucket TTL")??;
    assert!(
        registry.entry(&key).await?.is_none(),
        "TTL must remove the registration without a deletion marker"
    );
    Ok(())
}
