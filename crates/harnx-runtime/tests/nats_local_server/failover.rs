use super::*;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_execution_control::{ExecutionStore, Operation, OperationKind, OperationRef};
use harnx_nats_common::{connect::NatsEndpoint, rpc};
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use harnx_runtime::nats_local_server::{LocalBroker, SharedNatsServer};
use std::time::Duration;

async fn client(server: &SharedNatsServer) -> Result<async_nats::Client> {
    NatsEndpoint {
        url: server.url.clone(),
        token: Some(server.token.clone()),
        ..Default::default()
    }
    .connect()
    .await
}

async fn cancellation_observer(
    store: &ExecutionStore,
    reference: &OperationRef,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let cancel_store = store.clone();
    let cancel_reference = reference.clone();
    let mut updates = store.watch().await?;
    Ok(tokio::spawn(async move {
        while let Some(update) = updates.next().await {
            update?;
            cancel_store.check_ancestors(&cancel_reference).await?;
        }
        anyhow::bail!("cancellation watch closed")
    }))
}

async fn failover_lease(js: &async_nats::jetstream::Context) -> Result<NatsSessionLease> {
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "failover",
        worker_id: "survivor".into(),
        generation: 1,
        config: NatsLeaseConfig {
            ttl: Duration::from_secs(6),
            renew_interval: Duration::from_secs(1),
            ..Default::default()
        },
        session_metadata: None,
    })
    .await?
    .context("acquire lease")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_failover_preserves_live_clients_leases_and_cancellation() -> Result<()> {
    require_nextest();
    if skip_without_nats_server() {
        return Ok(());
    }
    let (_directory, _guard) = isolated_data_dir();
    let owner = ensure_shared_server().await?;
    let survivor = LocalBroker::start().await?;
    let client = client(&owner).await?;
    let js = async_nats::jetstream::new(client.clone());
    let lease = failover_lease(&js).await?;
    let store = ExecutionStore::ensure(&js, 1).await?;
    let reference = OperationRef::new("failover", "operation");
    store
        .create(&Operation::preparing(
            reference.clone(),
            OperationKind::Tool,
            None,
        ))
        .await?;
    let mut cancellation = cancellation_observer(&store, &reference).await?;
    let mut subscription = client.subscribe("surviving-client").await?;
    client.flush().await?;
    let old_identity = (owner.url.clone(), owner.token.clone(), owner.nonce.clone());
    drop(owner);
    tokio::time::timeout(Duration::from_secs(10), async {
        while survivor.status().nonce == old_identity.2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        client.flush().await
    })
    .await??;
    assert_eq!(
        (survivor.status().url, survivor.status().token),
        (old_identity.0, old_identity.1)
    );
    client
        .publish("surviving-client", "still subscribed".into())
        .await?;
    let message = tokio::time::timeout(Duration::from_secs(3), subscription.next())
        .await?
        .context("subscription closed")?;
    assert_eq!(message.payload.as_ref(), b"still subscribed");
    assert!(
        lease.revalidate_ownership().await?,
        "failover fenced a surviving worker"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(1200), &mut cancellation)
            .await
            .is_err(),
        "broker outage became cancellation"
    );
    store.cancel_operation(&reference, None, false).await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), cancellation)
            .await??
            .is_err(),
        "cancellation was missed after reconnect"
    );
    lease.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_flight_rpc_returns_after_broker_owner_exit_without_reinvocation() -> Result<()> {
    require_nextest();
    if skip_without_nats_server() {
        return Ok(());
    }
    let (_directory, _guard) = isolated_data_dir();
    let owner = ensure_shared_server().await?;
    let _survivor = LocalBroker::start().await?;
    let caller = client(&owner).await?;
    let responder = client(&owner).await?;
    let mut requests = responder.subscribe("failover-rpc").await?;
    responder.flush().await?;
    let waiting_parent = tokio::spawn(async move {
        rpc::request(
            &caller,
            "failover-rpc".into(),
            async_nats::Request::new()
                .payload("work".into())
                .timeout(None),
        )
        .await
    });
    let request = requests.next().await.context("request delivered")?;
    let _activity = rpc::RequestActivity::start(&responder, &request);
    let target = rpc::ReplyTarget::from_message(&request)?;
    drop(owner);
    let reply = tokio::spawn(async move { target.send(&responder, "finished").await });
    let result = tokio::time::timeout(Duration::from_secs(10), waiting_parent).await???;
    assert_eq!(result.payload.as_ref(), b"finished");
    tokio::time::timeout(Duration::from_secs(3), reply).await???;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), requests.next())
            .await
            .is_err(),
        "handler request was replayed"
    );
    Ok(())
}
