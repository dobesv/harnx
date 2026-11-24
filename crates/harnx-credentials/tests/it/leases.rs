use anyhow::Result;
use harnx_credentials::{CredentialMaterial, Error, RefreshLeaseStore};
use std::time::Duration;

use crate::common::*;

#[tokio::test]
async fn concurrent_lease_acquisition_has_exactly_one_holder() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js).await?;
    store.put_connect(0, connect("secret-marker")).await?;
    let first = RefreshLeaseStore::open(&js, 1).await?;
    let second = RefreshLeaseStore::open(&js, 1).await?;
    let (a, b) = tokio::join!(first.acquire(&store), second.acquire(&store));
    let winner = one_winner(a, b, Error::LeaseBusy);
    winner.release().await?;
    second.acquire(&store).await?.release().await?;
    Ok(())
}

#[tokio::test]
async fn refresh_commit_preserves_connection_absolute_expiry_and_rotated_token() -> Result<()> {
    let (_server, js) = nats().await?;
    let (store, original) = oauth_connection(&js, "access-original", "refresh-original").await?;
    let leases = RefreshLeaseStore::open(&js, 1).await?;
    let lease = leases.acquire(&store).await?;
    assert_eq!(lease.credential()?.revision, original.revision);
    let rotated = lease
        .commit(oauth("access-new", Some("refresh-rotated")))
        .await?;
    assert_eq!(rotated.record.connection_id, original.record.connection_id);
    assert_eq!(rotated.record.expires_at, original.record.expires_at);
    let lease = leases.acquire(&store).await?;
    let omitted = lease.commit(oauth("access-later", None)).await?;
    let CredentialMaterial::Oauth2(material) = omitted.record.material else {
        panic!("expected OAuth");
    };
    assert_eq!(material.refresh_token.unwrap().expose(), "refresh-rotated");
    let reconnect = store
        .put_connect(omitted.revision, connect("reconnect"))
        .await?;
    assert_ne!(
        reconnect.record.connection_id,
        original.record.connection_id
    );
    Ok(())
}

#[tokio::test]
async fn reconnect_and_disconnect_fence_refresh_commit() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js).await?;
    let original = store.put_connect(0, connect("secret-marker")).await?;
    let leases = RefreshLeaseStore::open(&js, 1).await?;
    let lease = leases.acquire(&store).await?;
    let reconnect = store
        .put_connect(original.revision, connect("new-connect"))
        .await?;
    assert_eq!(
        lease.commit(token("stale-refresh")).await.unwrap_err(),
        Error::Conflict
    );
    assert_eq!(
        store.get().await?.unwrap().record.connection_id,
        reconnect.record.connection_id
    );
    let lease = leases.acquire(&store).await?;
    store.delete(reconnect.revision).await?;
    assert_eq!(
        lease.commit(token("resurrection")).await.unwrap_err(),
        Error::Conflict
    );
    assert!(store.get().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn expired_holder_cannot_commit_or_release_new_holders_lease() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js).await?;
    let original = store.put_connect(0, connect("original")).await?;
    let leases = RefreshLeaseStore::open_with_ttl(&js, 1, Duration::from_millis(250)).await?;
    let expired = leases.acquire(&store).await?;
    tokio::time::sleep(Duration::from_millis(350)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let successor = loop {
        match leases.acquire(&store).await {
            Ok(lease) => break lease,
            Err(Error::LeaseBusy) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            other => return Err(anyhow::anyhow!("unexpected acquisition result: {other:?}")),
        }
    };
    assert_eq!(
        expired.commit(token("expired")).await.unwrap_err(),
        Error::LeaseLost
    );
    assert_eq!(store.get().await?.unwrap().revision, original.revision);
    assert_eq!(leases.acquire(&store).await.unwrap_err(), Error::LeaseBusy);
    successor.release().await?;
    Ok(())
}

#[tokio::test]
async fn drop_releases_lease_and_incompatible_ttl_is_rejected() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js).await?;
    store.put_connect(0, connect("secret")).await?;
    let leases = RefreshLeaseStore::open(&js, 1).await?;
    assert_eq!(
        RefreshLeaseStore::open_with_ttl(&js, 1, Duration::from_millis(250))
            .await
            .unwrap_err(),
        Error::Storage
    );
    drop(leases.acquire(&store).await?);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match leases.acquire(&store).await {
            Ok(lease) => {
                lease.release().await?;
                break;
            }
            Err(Error::LeaseBusy) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            other => return Err(anyhow::anyhow!("unexpected acquisition result: {other:?}")),
        }
    }
    Ok(())
}

// qual:allow(dry, fragment) reason: "same refresh fixture injects release failure instead of exercising token rotation"
#[tokio::test]
async fn successful_commit_is_not_masked_by_lease_cleanup_failure() -> Result<()> {
    let (_server, js) = nats().await?;
    let (store, original) = oauth_connection(&js, "access-before", "refresh-before").await?;
    let leases = RefreshLeaseStore::open(&js, 1).await?;
    let lease = leases.acquire(&store).await?;
    let locks = js.get_stream("KV_harnx_tool_credential_locks").await?;
    // The lock remains readable, but sealing refuses the release's delete publish.
    let mut config = locks.cached_info().config.clone();
    config.sealed = true;
    js.update_stream(config).await?;
    let committed = lease
        .commit(oauth("access-after", Some("refresh-after")))
        .await?;
    assert_eq!(
        committed.record.connection_id,
        original.record.connection_id
    );
    assert_eq!(store.get().await?.unwrap().revision, committed.revision);
    assert!(committed.revision > original.revision);
    assert_eq!(leases.acquire(&store).await.unwrap_err(), Error::LeaseBusy);
    Ok(())
}
