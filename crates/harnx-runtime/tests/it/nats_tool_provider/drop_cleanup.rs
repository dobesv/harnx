use super::*;
use futures_util::StreamExt;
use harnx_toolset::ControlMessage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Exposes the server's cancellation token only after the remote invocation starts.
struct ObservedTimeToolset {
    started: mpsc::UnboundedSender<CancellationToken>,
}

#[async_trait]
impl Toolset for ObservedTimeToolset {
    fn name(&self) -> &str {
        "time"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        TimeToolset::new().tools()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        let _ = self.started.send(cancel.clone());
        TimeToolset::new().invoke(tool, args, cancel).await
    }
}

struct DropFixture {
    provider: NatsToolProvider,
    calls: NatsInFlightCalls,
    controls: async_nats::Subscriber,
    started: mpsc::UnboundedReceiver<CancellationToken>,
    server_task: tokio::task::JoinHandle<Result<()>>,
    _env: EnvGuard,
    _server: common::NatsServerHandle,
}

impl Drop for DropFixture {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

impl DropFixture {
    async fn start() -> Result<Option<Self>> {
        let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
            auth_token: Some(TOKEN.to_string()),
        })
        .await?
        else {
            return Ok(None);
        };
        let instance_id = ServerScope::new();
        let env = EnvGuard::install(server.url(), TOKEN, &instance_id);
        let (started_tx, started) = mpsc::unbounded_channel();
        let server_url = server.url.clone();
        let server_instance = instance_id.clone();
        let server_task = tokio::spawn(async move {
            serve_over_nats(
                ObservedTimeToolset {
                    started: started_tx,
                },
                server_instance,
                &server_url,
                TOKEN,
            )
            .await
        });
        let client = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(server.url())
            .await?;
        wait_for_registry(&client, &instance_id, "____time").await?;
        let controls = client.subscribe(instance_id.control_subject()).await?;
        client.flush().await?;
        let calls = NatsInFlightCalls::for_instance(&instance_id);
        let provider =
            NatsToolProvider::discover(&Config::default(), instance_id, calls.clone(), None)
                .await?;
        Ok(Some(Self {
            provider,
            calls,
            controls,
            started,
            server_task,
            _env: env,
            _server: server,
        }))
    }

    async fn next_cancel(&mut self) -> Result<ControlMessage> {
        let message = tokio::time::timeout(Duration::from_secs(5), self.controls.next())
            .await
            .context("remote subscriber never received cancel")?
            .context("control subscription ended")?;
        Ok(serde_json::from_slice(&message.payload)?)
    }

    async fn assert_no_cancel(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), self.controls.next())
                .await
                .is_err(),
            "unexpected or duplicate remote cancel"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn timed_out_wait_removes_registration_and_cancels_remote_call() -> Result<()> {
    let Some(mut fixture) = DropFixture::start().await? else {
        return Ok(());
    };
    let abort = create_abort_signal();
    let mut call = Box::pin(
        fixture
            .provider
            .call_tool("wait", json!({"seconds": 300.0}), &abort),
    );
    let remote_cancel = tokio::select! {
        _ = &mut call => anyhow::bail!("call returned before timeout"),
        started = tokio::time::timeout(Duration::from_secs(5), fixture.started.recv()) => {
            started.context("remote call never started")?.context("tool server stopped")?
        }
    };
    let targets = fixture.calls.snapshot_for_session("").await;
    assert_eq!(targets.len(), 1);
    assert!(tokio::time::timeout(Duration::from_millis(10), call)
        .await
        .is_err());
    assert!(fixture.calls.snapshot_for_session("").await.is_empty());
    assert!(!abort.aborted(), "timeout must not abort the session");
    let control = fixture.next_cancel().await?;
    assert_eq!(control.call_id, targets[0].call_id);
    assert_eq!(control.operation_id, targets[0].call_id);
    assert_eq!(control.server, targets[0].server);
    assert_eq!(control.session_id, targets[0].call_id);
    tokio::time::timeout(Duration::from_secs(5), remote_cancel.cancelled())
        .await
        .context("remote tool did not receive cancellation")?;
    fixture.assert_no_cancel().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_failover_wait_does_not_cancel_remote_call() -> Result<()> {
    let Some(mut fixture) = DropFixture::start().await? else {
        return Ok(());
    };
    let abort = create_abort_signal();
    let mut call = Box::pin(
        fixture
            .provider
            .call_tool("wait", json!({"seconds": 300.0}), &abort),
    );
    let remote_cancel = tokio::select! {
        _ = &mut call => anyhow::bail!("call returned before failover"),
        started = tokio::time::timeout(Duration::from_secs(5), fixture.started.recv()) => {
            started.context("remote call never started")?.context("tool server stopped")?
        }
    };
    assert_eq!(fixture.calls.snapshot_for_session("").await.len(), 1);
    abort.set_failover();
    // Drop before the select can observe failover, as a worker teardown does.
    drop(call);
    assert!(fixture.calls.snapshot_for_session("").await.is_empty());
    fixture.assert_no_cancel().await;
    assert!(!remote_cancel.is_cancelled());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_wait_does_not_cancel_remote_call() -> Result<()> {
    let Some(mut fixture) = DropFixture::start().await? else {
        return Ok(());
    };
    fixture
        .provider
        .call_tool("wait", json!({"seconds": 0.01}), &create_abort_signal())
        .await
        .map_err(tool_error)?;
    assert!(fixture.calls.snapshot_for_session("").await.is_empty());
    fixture.assert_no_cancel().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_abort_cancels_remote_call_only_once() -> Result<()> {
    let Some(mut fixture) = DropFixture::start().await? else {
        return Ok(());
    };
    let abort = create_abort_signal();
    let mut call = Box::pin(
        fixture
            .provider
            .call_tool("wait", json!({"seconds": 300.0}), &abort),
    );
    let remote_cancel = tokio::select! {
        _ = &mut call => anyhow::bail!("call returned before abort"),
        started = tokio::time::timeout(Duration::from_secs(5), fixture.started.recv()) => {
            started.context("remote call never started")?.context("tool server stopped")?
        }
    };
    abort.set_ctrlc();
    assert!(call.await.is_err());
    assert!(fixture.calls.snapshot_for_session("").await.is_empty());
    fixture.next_cancel().await?;
    tokio::time::timeout(Duration::from_secs(5), remote_cancel.cancelled())
        .await
        .context("explicit abort did not cancel remote tool")?;
    fixture.assert_no_cancel().await;
    Ok(())
}
