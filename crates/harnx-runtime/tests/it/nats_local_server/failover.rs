use super::*;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_core::session::SessionLogEntry;
use harnx_nats_common::{connect::NatsEndpoint, rpc};
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use harnx_runtime::nats_local_server::{LocalBroker, SharedNatsServer};
use harnx_runtime::nats_session_log::NatsSessionLog;
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

const FAILOVER_SESSION: &str = "failover";

async fn failover_lease(js: &async_nats::jetstream::Context) -> Result<NatsSessionLease> {
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: FAILOVER_SESSION,
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
    // Interruption is durable state in the session log, so the broker handover
    // is what it has to survive.
    let log = NatsSessionLog::new_with_replicas(js.clone(), FAILOVER_SESSION, 1);
    let cancel_seq = log
        .append_event_async(&SessionLogEntry::cancel_request(
            "failover-cancellation".into(),
            "client:test".into(),
        ))
        .await?;
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
    // Only that the entry survived: nothing in this test appends a second
    // `Cancel`, so "the outage did not invent another" is not something the
    // handover could have got wrong, and asserting it would pass whatever
    // happened.
    let entries = log.load_events_latest_async().await?;
    assert!(
        entries
            .iter()
            .any(|(seq, entry)| *seq == cancel_seq
                && matches!(entry, SessionLogEntry::Cancel { .. })),
        "the interruption accepted before failover must read back unchanged: {entries:?}"
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
    let request = tokio::time::timeout(Duration::from_secs(5), requests.next())
        .await
        .context("request delivery timed out")?
        .context("request delivered")?;
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
