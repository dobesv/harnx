use super::*;
use harnx_toolset::{CancelAcceptance, CancellationAcknowledgement, ControlMessage};
use tokio::sync::Notify;

/// A hook that signals it has started, then blocks forever. Only a
/// cancellation ends the call; nothing here ever completes on its own.
#[derive(Default)]
struct BlockingHook {
    entered: Notify,
}

#[async_trait]
impl Hook for BlockingHook {
    fn name(&self) -> &str {
        "echo"
    }
    fn hooks(&self) -> Vec<HookSpec> {
        EchoHook.hooks()
    }
    async fn handle_hook(&self, _payload: HookPayload) -> HookOutcome {
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_control_message_interrupts_running_hook() -> Result<()> {
    harnx_core::require_nextest();
    let server = spawn_nats_server().await?.context("nats-server required")?;
    let client = connect_test_client(&server.url).await?;

    let hook = Arc::new(BlockingHook::default());
    let scope = ServerScope::new();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(serve_with_shutdown(
        hook.clone(),
        scope.clone(),
        NatsConnection {
            client: client.clone(),
            replicas: 1,
        },
        ServeLifecycle::new(shutdown.clone(), None),
    ));
    wait_for_registration(&client, &scope).await?;

    let session_id = "hook-session";
    let call_id = "blocking-hook-call";
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Harnx-Hook-Session", session_id);
    headers.insert("Harnx-Hook-Call", call_id);
    let payload = HookPayload {
        session_id: session_id.into(),
        cwd: std::env::current_dir()?,
        resume_count: 0,
        hook_event: HookEvent::PreToolUse {
            tool_name: "example".into(),
            tool_input: json!({"text": "x"}),
            tool_use_id: "call".into(),
        },
    };
    let invoke = client.send_request(
        scope.hook_subject("echo", "PreToolUse"),
        async_nats::Request::new()
            .headers(headers)
            .payload(serde_json::to_vec(&payload)?.into())
            .timeout(None),
    );
    tokio::pin!(invoke);
    tokio::select! {
        _ = hook.entered.notified() => {},
        result = &mut invoke => anyhow::bail!("hook finished early: {result:?}"),
    }

    let control = ControlMessage::cancel(
        "echo".into(),
        session_id.into(),
        call_id.into(),
        "c-1".into(),
    );
    let ack_message = client
        .request(
            scope.hook_control_subject("echo"),
            serde_json::to_vec(&control)?.into(),
        )
        .await?;
    let ack: CancellationAcknowledgement = serde_json::from_slice(&ack_message.payload)?;
    assert_eq!(ack.acceptance, CancelAcceptance::Accepted);

    let reply = tokio::time::timeout(Duration::from_secs(2), &mut invoke).await??;
    assert_interrupted_reply(reply)?;

    shutdown.cancel();
    task.await??;
    Ok(())
}

fn assert_interrupted_reply(reply: async_nats::Message) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(&reply.payload)?;
    let interrupted = value
        .get("interrupted")
        .context("expected an interrupted hook reply")?;
    assert_eq!(interrupted["cancellation_id"], "c-1");
    Ok(())
}
