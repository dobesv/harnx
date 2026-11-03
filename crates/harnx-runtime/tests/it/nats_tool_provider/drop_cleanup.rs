use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn timed_out_wait_removes_in_flight_registration() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let instance_id = ServerScope::new();
    let _env = EnvGuard::install(server.url(), TOKEN, &instance_id);
    let server_url = server.url.clone();
    let server_instance = instance_id.clone();
    let server_task = tokio::spawn(async move {
        serve_over_nats(TimeToolset::new(), server_instance, &server_url, TOKEN).await
    });
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    wait_for_registry(&client, &instance_id, "____time").await?;
    let calls = NatsInFlightCalls::for_instance(&instance_id);
    let provider =
        NatsToolProvider::discover(&Config::default(), instance_id, calls.clone(), None).await?;
    assert_dropped_wait_cleans_registration(&provider, &calls).await?;
    server_task.abort();
    Ok(())
}

async fn assert_dropped_wait_cleans_registration(
    provider: &NatsToolProvider,
    calls: &NatsInFlightCalls,
) -> Result<()> {
    let abort = create_abort_signal();
    let mut call = Box::pin(provider.call_tool("wait", json!({"seconds": 300.0}), &abort));
    // Poll the real provider until await_response has registered, rather than
    // assuming a fixed sleep gives discovery/journal setup time to finish.
    tokio::select! {
        _ = &mut call => anyhow::bail!("call returned before timeout"),
        result = tokio::time::timeout(Duration::from_secs(5), async {
            while calls.snapshot_for_session("").await.is_empty() {
                tokio::task::yield_now().await;
            }
        }) => result.context("call never entered in_flight")?,
    }
    assert_eq!(calls.snapshot_for_session("").await.len(), 1);
    assert!(tokio::time::timeout(Duration::from_millis(10), call)
        .await
        .is_err());
    assert!(
        calls.snapshot_for_session("").await.is_empty(),
        "dropped wait leaked its registration"
    );
    assert!(!abort.aborted(), "timeout must not abort the session");
    Ok(())
}
