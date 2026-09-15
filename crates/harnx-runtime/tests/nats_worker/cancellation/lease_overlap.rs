use super::*;
use harnx_execution_control::{InterruptScope, OperationRef, Owner};
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsSessionLease};

async fn lease(
    js: &async_nats::jetstream::Context,
    session_id: &str,
    execution_id: Option<&str>,
) -> Result<Option<NatsSessionLease>> {
    NatsSessionLease::acquire_for_execution(
        NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id,
            worker_id: uuid::Uuid::new_v4().to_string(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: Duration::from_secs(300),
                renew_interval: Duration::from_secs(90),
                ..Default::default()
            },
            session_metadata: None,
        },
        execution_id.map(str::to_owned),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_stop_replaces_unexpired_lease_and_old_release_cannot_delete_g2() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client);
    let store = harnx_execution_control::ExecutionStore::ensure(&js, 1).await?;
    let session_id = "crash-after-acceptance-lease";
    let g1 = store.session(session_id, None, Some("g1")).await?;
    let old = lease(&js, session_id, Some("g1")).await?.unwrap();
    store
        .claim(
            &g1.reference,
            Owner {
                instance_id: old.worker_id().into(),
                fence: old.fence_token(),
            },
        )
        .await?;
    let context = store.activate_gate(&g1.reference).await?;
    assert!(
        lease(&js, session_id, Some("g2")).await?.is_none(),
        "cannot steal live execution"
    );
    assert!(
        old.revalidate_ownership().await?,
        "renewal must retain execution binding"
    );
    store
        .interrupt(
            &InterruptScope {
                gate_root: context.gate_root().clone(),
                operation: g1.reference,
                reason: "crash immediately after acceptance".into(),
            },
            "stop-g1",
        )
        .await?;

    assert!(
        lease(&js, session_id, None).await?.is_none(),
        "an unbound requester cannot revoke even a stopped execution's lease"
    );
    assert!(old.revalidate_ownership().await?);
    let g2 = store.session(session_id, None, Some("g2")).await?;
    // No old release, owner confirmation or 300-second expiry occurs first.
    let new = lease(&js, session_id, Some("g2"))
        .await?
        .expect("accepted stop releases execution control");
    store
        .claim(
            &g2.reference,
            Owner {
                instance_id: new.worker_id().into(),
                fence: new.fence_token(),
            },
        )
        .await?;
    store.activate_gate(&g2.reference).await?;
    assert!(!old.revalidate_ownership().await?);
    old.release().await?;
    assert!(new.revalidate_ownership().await?);
    assert!(
        lease(&js, session_id, Some("g1")).await?.is_none(),
        "old stop is not G2 stop evidence"
    );
    new.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unbound_legacy_lease_is_not_revoked_from_session_current_stop() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client);
    let store = harnx_execution_control::ExecutionStore::ensure(&js, 1).await?;
    let session_id = "unbound-legacy-lease";
    let g1 = store.session(session_id, None, Some("g1")).await?;
    let old = lease(&js, session_id, None).await?.unwrap();
    store
        .cancel_operation(&g1.reference, Some("stop"), false)
        .await?;
    assert!(lease(&js, session_id, Some("g1")).await?.is_none());
    old.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_registration_cancel_permits_cleanup_lease_takeover_without_expiry() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client);
    let store = harnx_execution_control::ExecutionStore::ensure(&js, 1).await?;
    let session_id = "cancel-before-lease-owner-claim";
    store.session(session_id, None, Some("g1")).await?;
    let old = lease(&js, session_id, Some("g1")).await?.unwrap();
    let reference = OperationRef::new(session_id, "g1");
    store
        .cancel_operation(&reference, Some("stop"), false)
        .await?;
    let recovery = lease(&js, session_id, Some("g1")).await?.unwrap();
    assert!(!store.get(&reference).await?.unwrap().allows_continuation());
    old.release().await?;
    assert!(recovery.revalidate_ownership().await?);
    recovery.release().await?;
    Ok(())
}
