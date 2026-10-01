//! A failed call returns the partial result its tool recorded; a successful
//! one returns only its own result.
use super::*;
use harnx_core::partial_result::partial_result_of;
use harnx_toolset::ToolInvocation;
use tokio::sync::Notify;

/// Every tool records `{"job": <tool name>}` as its partial result, then
/// fails, waits to be cut off, or succeeds.
struct PartialResultToolset {
    /// Signalled once `lose_after_partial` has recorded its partial result.
    recorded: Arc<Notify>,
}

#[async_trait]
impl Toolset for PartialResultToolset {
    fn name(&self) -> &str {
        "partial"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        [
            "fail_after_partial",
            "lose_after_partial",
            "succeed_after_partial",
        ]
        .into_iter()
        .map(|name| ToolSpec {
            cancellation_guarantee: Default::default(),
            name: name.to_string(),
            description: format!("{name} test tool"),
            input_schema: json!({"type": "object"}),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: None,
            meta: None,
        })
        .collect()
    }

    async fn invoke(
        &self,
        tool: &str,
        _args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        Err(ToolInvokeError::Fatal(format!(
            "{tool} needs its invocation context"
        )))
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        invocation
            .context
            .record_partial_result(json!({"job": invocation.tool}))
            .await
            .map_err(|error| ToolInvokeError::Fatal(format!("{error:#}")))?;
        match invocation.tool.as_str() {
            "fail_after_partial" => Err(ToolInvokeError::Recoverable("job failed".into())),
            "lose_after_partial" => {
                self.recorded.notify_one();
                invocation.cancel.cancelled().await;
                Err(ToolInvokeError::Fatal("cancelled".into()))
            }
            _ => Ok(json!({"done": true})),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_call_returns_the_partial_result_its_tool_recorded() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let instance_id = ServerScope::new();
    let _env = EnvGuard::install(server.url(), TOKEN, &instance_id);
    let recorded = Arc::new(Notify::new());
    let toolset = PartialResultToolset {
        recorded: Arc::clone(&recorded),
    };
    let server_url = server.url.clone();
    let server_instance = instance_id.clone();
    let server_task =
        tokio::spawn(
            async move { serve_over_nats(toolset, server_instance, &server_url, TOKEN).await },
        );
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    wait_for_registry(&client, &instance_id, "____partial").await?;
    let provider = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(&instance_id),
        None,
    )
    .await?;

    assert_failed_call_returns_its_partial_result(&provider).await?;
    assert_lost_server_returns_its_partial_result(&provider, recorded).await?;
    assert_successful_call_returns_only_its_own_result(&provider).await?;

    server_task.abort();
    let _ = server_task.await;
    Ok(())
}

async fn assert_failed_call_returns_its_partial_result(provider: &NatsToolProvider) -> Result<()> {
    let Err(ToolError::Recoverable(failed)) = provider
        .call_tool("fail_after_partial", json!({}), &create_abort_signal())
        .await
    else {
        anyhow::bail!("a failing tool returns a recoverable error");
    };
    assert_eq!(format!("{failed:#}"), "job failed");
    assert_eq!(
        partial_result_of(&failed),
        Some(&json!({"job": "fail_after_partial"}))
    );
    Ok(())
}

/// The tool server is reported gone mid-call, after the tool recorded its
/// partial result, the way the tool supervisor reports a crashed process.
async fn assert_lost_server_returns_its_partial_result(
    provider: &NatsToolProvider,
    recorded: Arc<Notify>,
) -> Result<()> {
    let in_flight = provider.in_flight_calls();
    let lose_server = tokio::spawn(async move {
        recorded.notified().await;
        in_flight
            .fail_server_unavailable("____partial", "partial tool process exited")
            .await;
    });
    let lost = provider
        .call_tool("lose_after_partial", json!({}), &create_abort_signal())
        .await;
    lose_server.await?;
    let Err(ToolError::Recoverable(lost)) = lost else {
        anyhow::bail!("losing the tool server returns a recoverable error");
    };
    assert_eq!(lost.to_string(), "partial tool process exited");
    assert_eq!(
        partial_result_of(&lost),
        Some(&json!({"job": "lose_after_partial"}))
    );
    Ok(())
}

async fn assert_successful_call_returns_only_its_own_result(
    provider: &NatsToolProvider,
) -> Result<()> {
    let succeeded = provider
        .call_tool("succeed_after_partial", json!({}), &create_abort_signal())
        .await
        .map_err(tool_error)?;
    assert_eq!(
        succeeded.value,
        json!({"done": true}),
        "a successful call returns only its own result"
    );
    Ok(())
}
