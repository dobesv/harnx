use super::*;
use harnx_execution_control::{ExecutionStore, OperationRef, OperationState, Owner};
use tokio::sync::Notify;

#[derive(Default)]
struct GatedHook {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl Hook for GatedHook {
    fn name(&self) -> &str {
        "echo"
    }
    fn hooks(&self) -> Vec<HookSpec> {
        EchoHook.hooks()
    }
    async fn handle_hook(&self, payload: HookPayload) -> HookOutcome {
        self.entered.notify_one();
        self.release.notified().await;
        EchoHook.handle_hook(payload).await
    }
}

async fn cancel_and_expire(store: &ExecutionStore, reference: &OperationRef) -> Result<()> {
    store.cancel_operation(reference, None, false).await?;
    store
        .mutate(reference, |operation| {
            operation.cancellation.as_mut().unwrap().progress_at =
                operation.created_at - Duration::from_secs(6);
            Ok(())
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn parent_cancellation_waits_for_blocking_hook_future() -> Result<()> {
    harnx_core::require_nextest();
    let server = spawn_nats_server().await?.context("nats-server required")?;
    let client = connect_test_client(&server.url).await?;
    let store = ExecutionStore::ensure(&async_nats::jetstream::new(client.clone()), 1).await?;
    let root = store.session("hook-parent", None, None).await?;
    let owner = Owner {
        instance_id: "worker".into(),
        fence: 1,
    };
    store.claim(&root.reference, owner.clone()).await?;
    let child = OperationRef::new("hook-parent", "blocking-hook");
    store.child(child.clone(), root.reference.clone()).await?;
    let hook = Arc::new(GatedHook::default());
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
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(
        "Harnx-Hook-Operation",
        serde_json::to_string(&child)?.as_str(),
    );
    let payload = HookPayload {
        session_id: "hook-parent".into(),
        cwd: std::env::current_dir()?,
        resume_count: 0,
        hook_event: HookEvent::PreToolUse {
            tool_name: "example".into(),
            tool_input: json!({}),
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
    cancel_and_expire(&store, &root.reference).await?;
    store
        .record_coverage(&root.reference, &owner, 0, true)
        .await?;
    store.owner_stopped(&root.reference, &owner).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut invoke)
            .await
            .is_err()
    );
    assert!(!store.get(&child).await?.unwrap().owner_stopped);
    assert_eq!(
        store.status(&root.reference).await?.state,
        OperationState::Unconfirmed
    );
    hook.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), &mut invoke).await??;
    assert_eq!(
        store.status(&root.reference).await?.state,
        OperationState::Cancelled
    );
    shutdown.cancel();
    task.await??;
    Ok(())
}
