use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn nats_tool_provider_reports_backend_outage_during_unbounded_call() -> Result<()> {
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
    set_wait_timeout(&client, &instance_id, 0).await?;
    let provider = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(&instance_id),
        None,
    )
    .await?;
    let abort = create_abort_signal();
    let call = provider.call_tool("wait", json!({"seconds": 300.0}), &abort);
    tokio::pin!(call);
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
        _ = &mut call => anyhow::bail!("call returned before the outage"),
    }
    drop(server);
    let result = tokio::time::timeout(Duration::from_secs(40), call).await;
    server_task.abort();
    let result =
        result.context("unbounded tool request ignored persistent registry read failures")?;
    let Err(ToolError::Recoverable(error)) = result else {
        anyhow::bail!("expected explicit unavailable result")
    };
    assert!(
        error.to_string().contains("completion unconfirmed"),
        "{error:#}"
    );
    Ok(())
}
