use anyhow::Result;
use chrono::{TimeDelta, Utc};
use harnx_credentials::{
    envelope, store::CREDENTIALS_BUCKET, CredentialMaterial, CredentialStore, Error, Principal,
    Secret,
};
use std::time::Duration;

use crate::common::*;

#[tokio::test]
async fn cas_connect_refresh_disconnect_and_tombstone() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js).await?;
    let initial = store.put_connect(0, connect("first-secret-marker")).await?;
    let recovered = store.get().await?.unwrap();
    assert_eq!(recovered.revision, initial.revision);
    let CredentialMaterial::Token(token) = recovered.record.material else {
        panic!("expected token");
    };
    assert_eq!(token.token.expose(), "first-secret-marker");
    let rotated = store
        .put_refresh(
            initial.revision,
            crate::common::token("rotated-secret-marker"),
        )
        .await?;
    assert_eq!(rotated.record.connection_id, initial.record.connection_id);
    assert_eq!(rotated.record.expires_at, initial.record.expires_at);
    assert_eq!(
        store
            .put_connect(initial.revision, connect("stale"))
            .await
            .unwrap_err(),
        Error::Conflict
    );
    assert_eq!(
        store.mark_needs_login(initial.revision).await.unwrap_err(),
        Error::Conflict
    );
    assert_eq!(
        store.delete(initial.revision).await.unwrap_err(),
        Error::Conflict
    );
    let reconnect = store
        .put_connect(rotated.revision, connect("reconnect"))
        .await?;
    assert_ne!(reconnect.record.connection_id, rotated.record.connection_id);
    store.delete(reconnect.revision).await?;
    assert!(store.get().await?.is_none());
    assert_eq!(
        store
            .put_connect(0, connect("stale-create"))
            .await
            .unwrap_err(),
        Error::Conflict
    );
    let tombstone = store.inspect().await?;
    assert!(tombstone.record.is_none());
    assert!(tombstone.revision > reconnect.revision);
    store
        .put_connect(tombstone.revision, connect("new-connect"))
        .await?;
    Ok(())
}

#[tokio::test]
async fn absolute_expiry_and_needs_login_are_enforced_without_bucket_cleanup() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js).await?;
    let initial = store.put_connect(0, connect("secret-marker")).await?;
    let ttl = initial.record.expires_at - initial.record.created_at;
    assert_eq!(ttl, TimeDelta::days(365));
    let marked = store.mark_needs_login(initial.revision).await?;
    assert_eq!(store.get().await.unwrap_err(), Error::NeedsLogin);
    assert_eq!(marked.record.connection_id, initial.record.connection_id);
    let mut expired = marked.record;
    expired.expires_at = Utc::now() - TimeDelta::seconds(1);
    let kv = js.get_key_value(CREDENTIALS_BUCKET).await?;
    let bytes = envelope::seal(
        keyring()?.as_ref(),
        CREDENTIALS_BUCKET,
        &store.key(),
        &expired,
    )?;
    kv.update(store.key(), bytes.into(), marked.revision)
        .await?;
    assert_eq!(store.get().await.unwrap_err(), Error::Expired);
    assert!(store.inspect().await?.record.is_some());
    assert!(store.list_active_principals().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn configured_expiry_is_default_and_maximum() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js)
        .await?
        .with_credential_ttl(Duration::from_secs(3600))?;
    let mut input = connect("secret-marker");
    input.expires_at = Some(Utc::now() - TimeDelta::seconds(1));
    assert_eq!(
        store.put_connect(0, input.clone()).await.unwrap_err(),
        Error::Expired
    );
    input.expires_at = Some(Utc::now() + TimeDelta::hours(2));
    assert_eq!(
        store.put_connect(0, input.clone()).await.unwrap_err(),
        Error::InvalidRecord
    );
    input.expires_at = Some(Utc::now() + TimeDelta::minutes(10));
    let written = store.put_connect(0, input.clone()).await?;
    assert_eq!(written.record.expires_at, input.expires_at.unwrap());
    assert!(credentials(&js)
        .await?
        .with_credential_ttl(Duration::ZERO)
        .is_err());
    Ok(())
}

#[tokio::test]
async fn ownerless_and_similar_user_have_distinct_records_and_listing() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js).await?;
    let ownerless = store.for_principal(Principal::Ownerless)?;
    let similar = store.for_principal(Principal::User("=".into()))?;
    assert_eq!(ownerless.key(), "v1.amlyYQ.=");
    assert_eq!(similar.key(), "v1.amlyYQ.PQ");
    ownerless
        .put_connect(0, connect("ownerless-secret"))
        .await?;
    similar.put_connect(0, connect("similar-secret")).await?;
    store.put_connect(0, connect("alice-secret")).await?;
    assert_eq!(
        store
            .list_for_user(&Principal::User("=".into()))
            .await?
            .len(),
        1
    );
    assert_eq!(store.list_active_principals().await?.len(), 3);
    assert_eq!(ownerless.get().await?.unwrap().record.owner_user_id, None);
    assert_eq!(
        similar
            .get()
            .await?
            .unwrap()
            .record
            .owner_user_id
            .as_deref(),
        Some("=")
    );
    Ok(())
}

#[tokio::test]
async fn copied_nats_ciphertext_fails_closed_across_users_and_servers() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = credentials(&js).await?;
    store.put_connect(0, connect("token-secret-marker")).await?;
    let kv = js.get_key_value(CREDENTIALS_BUCKET).await?;
    let bytes = kv.get(store.key()).await?.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("token-secret-marker"));
    let bob = store.for_principal(Principal::User("bob".into()))?;
    kv.put(bob.key(), bytes.clone()).await?;
    assert_eq!(bob.get().await.unwrap_err(), Error::AuthenticationFailed);
    let other = CredentialStore::open(
        &js,
        1,
        keyring()?,
        "github",
        Principal::User("alice".into()),
    )
    .await?;
    kv.put(other.key(), bytes).await?;
    assert_eq!(other.get().await.unwrap_err(), Error::AuthenticationFailed);
    Ok(())
}

#[tokio::test]
async fn every_secret_is_redacted_in_record_flow_store_and_lease_debug() -> Result<()> {
    let (_server, js) = nats().await?;
    let (store, record) =
        oauth_connection(&js, "access-secret-marker", "refresh-secret-marker").await?;
    let flows = harnx_credentials::PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let flow = flows.create(Principal::Ownerless, flow_options()).await?;
    let leases = harnx_credentials::RefreshLeaseStore::open(&js, 1).await?;
    let lease = leases.acquire(&store).await?;
    let debug = format!(
        "{record:?} {flow:?} {store:?} {flows:?} {leases:?} {lease:?} {:?} {}",
        keyring()?,
        Secret::from("display-secret-marker")
    );
    for secret in [
        "access-secret-marker",
        "refresh-secret-marker",
        "client-secret-marker",
        "registration-secret-marker",
        "vendor-secret-marker",
        "pkce-secret-marker",
        "display-secret-marker",
        flow.record.challenge_id.expose(),
        flow.record.oauth_state.expose(),
        flow.record.csrf_token.expose(),
    ] {
        assert!(!debug.contains(secret), "secret leaked into Debug");
    }
    lease.release().await?;
    Ok(())
}
