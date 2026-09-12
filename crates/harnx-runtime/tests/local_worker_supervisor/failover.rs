use super::*;
use anyhow::{Context, Result};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_parent_receives_worker_result_across_broker_owner_exit() -> Result<()> {
    require_nextest();
    let Some(binary) = skip_without_binaries() else {
        return Ok(());
    };
    let root = tempfile::tempdir()?;
    let _environment = isolated_environment(root.path());
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, gated) = tokio::sync::oneshot::channel();
    let (url, mock) = start_mock_openai(Some((entered, gated))).await;
    write_trivial_agent_config(root.path(), &url);
    let owner =
        LocalWorkerSupervisor::start_with_worker_binary(&binary, create_abort_signal()).await?;
    let survivor =
        LocalWorkerSupervisor::start_with_worker_binary(&binary, create_abort_signal()).await?;
    let original_pid = survivor.worker_pid();
    let client = harnx_nats_common::connect::NatsEndpoint {
        url: survivor.server().url,
        token: Some(survivor.server().token),
        ..Default::default()
    }
    .connect()
    .await?;
    let js = async_nats::jetstream::new(client.clone());
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: LOCAL_CLUSTER_KEY.into(),
            initializer: harnx_runtime::SessionInitializer::named("trivial", Default::default()),
            session_id: Some("live-worker-failover".into()),
            activation_route: survivor.route().activation_route(),
        },
        client.clone(),
        js,
        create_abort_signal(),
    )
    .await?;
    let parent =
        tokio::spawn(async move { session.run_turn("hello", Arc::new(NullSink), None).await });
    tokio::time::timeout(Duration::from_secs(10), waiting).await??;
    drop(owner);
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.connection_state() == async_nats::connection::State::Connected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .context("observe broker disconnection")?;
    release.send(()).expect("complete model during outage");
    let result = tokio::time::timeout(Duration::from_secs(25), parent).await???;
    assert_eq!(result.response.as_deref(), Some("worker completed"));
    assert_eq!(result.error, None);
    assert!(!result.was_cancelled);
    assert_eq!(survivor.worker_pid(), original_pid);
    mock.await?;
    Ok(())
}
