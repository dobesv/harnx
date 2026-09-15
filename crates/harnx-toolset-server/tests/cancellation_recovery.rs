mod common;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_execution_control::{
    CleanupState, CleanupStatus, ExecutionContext, ExecutionStore, InterruptScope, OperationRef,
    Owner, StopReceipt,
};
use harnx_toolset::{CancelAcceptance, ControlMessage};
use harnx_toolset_server::cancellation_client::request_cancellation;
use std::time::Duration;

struct Fixture {
    _server: common::NatsServerHandle,
    client: async_nats::Client,
    store: ExecutionStore,
    root: ExecutionContext,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let client = async_nats::ConnectOptions::new()
            .token(common::TOKEN.into())
            .connect(&server.url)
            .await?;
        let js = async_nats::jetstream::new(client.clone());
        let store = ExecutionStore::ensure(&js, 1).await?;
        let root = store
            .open_gate(
                OperationRef::new("lost-ack", "g1"),
                Owner::invocation("worker"),
            )
            .await?;
        Ok(Self {
            _server: server,
            client,
            store,
            root,
        })
    }

    fn spawn_delayed_owner(
        &self,
        mut commands: async_nats::Subscriber,
        accepted: tokio::sync::oneshot::Sender<StopReceipt>,
        blocked: tokio::sync::oneshot::Receiver<()>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let store = self.store.clone();
        let client = self.client.clone();
        let root = self.root.clone();
        tokio::spawn(async move {
            let first = commands.next().await.context("control request")?;
            let request: ControlMessage = serde_json::from_slice(&first.payload)?;
            let scope = InterruptScope {
                gate_root: root.gate_root().clone(),
                operation: root.operation().clone(),
                reason: "durable acceptance".into(),
            };
            let stop = store.interrupt(&scope, &request.cancellation_id).await?;
            accepted.send(stop.clone()).unwrap();
            blocked.await?;
            // No first acknowledgement: simulate broker delivery loss after CAS.
            let retry = commands.next().await.context("retry request")?;
            let retry_control: ControlMessage = serde_json::from_slice(&retry.payload)?;
            assert_eq!(request, retry_control);
            let recovered = store
                .interrupt(&scope, &retry_control.cancellation_id)
                .await?;
            assert_eq!(recovered, stop);
            let ack = retry_control.acknowledgement(
                CancelAcceptance::Accepted { stop },
                Some(store.gate_cleanup(&root).await?),
            );
            client
                .publish(
                    retry.reply.context("retry inbox")?,
                    serde_json::to_vec(&ack)?.into(),
                )
                .await?;
            Ok(())
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_acceptance_ack_is_unknown_but_retry_recovers_durable_stop_not_cleanup() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let Fixture {
        client,
        store,
        root,
        ..
    } = &fixture;
    let commands = client.subscribe("test.delayed.control").await?;
    client.flush().await?;
    let control = ControlMessage::cancel(
        root.clone(),
        "delayed-owner".into(),
        "stable-cancellation".into(),
    );
    let (accepted, persisted) = tokio::sync::oneshot::channel();
    let (release, blocked) = tokio::sync::oneshot::channel();
    let server_task = fixture.spawn_delayed_owner(commands, accepted, blocked);
    let first = tokio::spawn({
        let client = client.clone();
        let control = control.clone();
        async move {
            request_cancellation(
                &client,
                "test.delayed.control".into(),
                &control,
                Duration::from_secs(1),
            )
            .await
        }
    });
    let stop = persisted.await?;
    let unknown = first.await?;
    assert!(matches!(
        unknown.acceptance,
        CancelAcceptance::Unknown { .. }
    ));
    assert!(unknown.cleanup.is_none());
    assert_eq!(
        store.gate_stop(root.gate_root(), root.operation()).await?,
        Some(stop.clone())
    );
    assert_eq!(store.gate_cleanup(root).await?.state, CleanupState::Pending);
    // Cleanup evidence can be Unconfirmed while stop acceptance is known exactly.
    store
        .record_cleanup(
            root,
            CleanupStatus::unconfirmed("owner has not reported shutdown"),
        )
        .await?;
    release.send(()).unwrap();
    let ack = request_cancellation(
        client,
        "test.delayed.control".into(),
        &control,
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(ack.acceptance, CancelAcceptance::Accepted { stop });
    assert_eq!(ack.cleanup.unwrap().state, CleanupState::Unconfirmed);
    server_task.await??;
    Ok(())
}
