use anyhow::Result;
use harnx_credentials::{
    rmcp_adapters::{RmcpCredentialStore, RmcpStateStore},
    CredentialMaterial, Error, NewCredential, PendingFlowStore, Principal, RefreshLeaseStore,
    Secret,
};
use oauth2::{AccessToken, RefreshToken, TokenResponse};
use rmcp::transport::auth::{CredentialStore as _, StateStore as _};

use crate::common::*;

#[tokio::test]
async fn rmcp_refresh_adapter_rotates_tokens_preserves_registration_and_rejects_unguarded_save(
) -> Result<()> {
    let (_server, js) = nats().await?;
    let (store, original, adapter) = oauth_adapter(&js).await?;
    let mut loaded = adapter.load().await?.unwrap();
    assert_eq!(
        loaded
            .token_response
            .as_ref()
            .unwrap()
            .access_token()
            .secret(),
        "access-original"
    );
    assert!(adapter.save(loaded.clone()).await.is_err());
    let guard = adapter.acquire_refresh_guard().await?.unwrap();
    let response = loaded.token_response.as_mut().unwrap();
    response.set_access_token(AccessToken::new("access-rotated".into()));
    response.set_refresh_token(Some(RefreshToken::new("refresh-rotated".into())));
    adapter.save(loaded).await?;
    drop(guard);
    let updated = store.get().await?.unwrap();
    assert_eq!(updated.record.connection_id, original.record.connection_id);
    assert_eq!(updated.record.expires_at, original.record.expires_at);
    let CredentialMaterial::Oauth2(material) = updated.record.material else {
        panic!("expected OAuth");
    };
    assert_eq!(material.access_token.expose(), "access-rotated");
    assert_eq!(material.refresh_token.unwrap().expose(), "refresh-rotated");
    assert_eq!(
        material.client.client_secret.unwrap().expose(),
        "client-secret-marker"
    );
    assert_eq!(
        material
            .client
            .registration
            .unwrap()
            .expose()
            .additional_fields["registration_access_token"],
        "registration-secret-marker"
    );
    assert_eq!(
        material.extra_token_fields.expose()["vendor_secret"],
        "vendor-secret-marker"
    );
    assert!(!format!("{adapter:?}").contains("secret-marker"));
    Ok(())
}

#[tokio::test]
async fn rmcp_refresh_rejection_marks_needs_login_and_old_adapter_cannot_touch_reconnect(
) -> Result<()> {
    let (_server, js) = nats().await?;
    let (store, _original, adapter) = oauth_adapter(&js).await?;
    let loaded = adapter.load().await?.unwrap();
    let guard = adapter.acquire_refresh_guard().await?.unwrap();
    adapter.on_refresh_token_rejected(&loaded).await?;
    drop(guard);
    assert_eq!(store.get().await.unwrap_err(), Error::NeedsLogin);
    let marked = store.inspect().await?;
    let reconnect = store
        .put_connect(
            marked.revision,
            NewCredential {
                material: oauth("new-access", Some("new-refresh")),
                ..connect("unused")
            },
        )
        .await?;
    assert!(adapter.load().await.is_err());
    assert!(adapter.clear().await.is_err());
    assert!(adapter.acquire_refresh_guard().await.is_err());
    assert_eq!(store.get().await?.unwrap().revision, reconnect.revision);
    Ok(())
}

// qual:allow(dry, fragment) reason: "same load/guard setup exercises a competing write rather than token rejection"
#[tokio::test]
async fn rmcp_guard_save_is_fenced_to_revision_at_acquisition() -> Result<()> {
    let (_server, js) = nats().await?;
    let (store, original, adapter) = oauth_adapter(&js).await?;
    let loaded = adapter.load().await?.unwrap();
    let guard = adapter.acquire_refresh_guard().await?.unwrap();
    let new = store
        .put_connect(
            original.revision,
            NewCredential {
                material: oauth("new-access", Some("new-refresh")),
                ..connect("unused")
            },
        )
        .await?;
    assert!(adapter.save(loaded).await.is_err());
    drop(guard);
    assert_eq!(store.get().await?.unwrap().revision, new.revision);
    Ok(())
}

#[tokio::test]
async fn rmcp_state_adapter_is_durable_and_cannot_access_another_flow() -> Result<()> {
    let (_server, js) = nats().await?;
    let flows = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let options = flow_options();
    let state = options
        .authorization_state
        .as_ref()
        .unwrap()
        .expose()
        .clone();
    let flow = flows.create(Principal::Ownerless, options).await?;
    let adapter = RmcpStateStore::new(flows.clone(), flow.record.challenge_id.clone());
    let loaded = adapter.load(&state.csrf_token).await?.unwrap();
    assert_eq!(loaded.pkce_verifier, state.pkce_verifier);
    assert!(adapter
        .save("different-state", state.clone())
        .await
        .is_err());
    let mut replaced_pkce = state.clone();
    replaced_pkce.pkce_verifier = "replacement-secret-marker".into();
    assert!(adapter
        .save(&state.csrf_token, replaced_pkce)
        .await
        .is_err());
    let mut weaker_issuer_check = state.clone();
    weaker_issuer_check.require_issuer = false;
    assert!(adapter
        .save(&state.csrf_token, weaker_issuer_check)
        .await
        .is_err());
    adapter.save(&state.csrf_token, state.clone()).await?;
    let second_flows = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let second = RmcpStateStore::new(second_flows, flow.record.challenge_id);
    assert_eq!(
        second.load(&state.csrf_token).await?.unwrap().pkce_verifier,
        "pkce-secret-marker"
    );
    let intruder = RmcpStateStore::new(flows, Secret::from("other-challenge"));
    assert!(intruder.load(&state.csrf_token).await.is_err());
    assert!(intruder.delete(&state.csrf_token).await.is_err());
    second.delete(&state.csrf_token).await?;
    assert!(second.load(&state.csrf_token).await?.is_none());
    assert!(second.load("unknown-state").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn expected_revision_refuses_refresh_after_another_instance_rotates() -> Result<()> {
    let (_server, js) = nats().await?;
    let (store, original, adapter) = oauth_adapter(&js).await?;
    let leases = RefreshLeaseStore::open(&js, 1).await?;
    let stale =
        RmcpCredentialStore::new(store.clone(), leases, original.record.connection_id.clone())
            .with_expected_revision(original.revision);
    let guard = adapter.acquire_refresh_guard().await?.unwrap();
    adapter.save(adapter.load().await?.unwrap()).await?;
    drop(guard);
    let rotated = store.get().await?.unwrap();
    assert!(rotated.revision > original.revision);
    let error = stale.acquire_refresh_guard().await.err().unwrap();
    assert!(matches!(
        error,
        rmcp::transport::auth::AuthError::CredentialStoreError(reason)
            if reason == harnx_credentials::Error::Conflict.to_string()
    ));
    assert_eq!(store.get().await?.unwrap().revision, rotated.revision);
    Ok(())
}

#[tokio::test]
async fn rmcp_adapter_discards_tokens_when_refresh_lease_expires() -> Result<()> {
    let (_server, js) = nats().await?;
    let (store, original) = oauth_connection(&js, "access-before", "refresh-before").await?;
    let leases =
        RefreshLeaseStore::open_with_ttl(&js, 1, std::time::Duration::from_millis(100)).await?;
    let adapter = RmcpCredentialStore::new(store.clone(), leases, original.record.connection_id);
    let guard = adapter.acquire_refresh_guard().await?.unwrap();
    let loaded = adapter.load().await?.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(adapter.save(loaded).await.is_err());
    drop(guard);
    assert_eq!(store.get().await?.unwrap().revision, original.revision);
    Ok(())
}

async fn oauth_adapter(
    js: &async_nats::jetstream::Context,
) -> Result<(
    harnx_credentials::CredentialStore,
    harnx_credentials::Versioned<harnx_credentials::CredentialRecord>,
    RmcpCredentialStore,
)> {
    let (store, record) = oauth_connection(js, "access-original", "refresh-original").await?;
    let leases = RefreshLeaseStore::open(js, 1).await?;
    let adapter =
        RmcpCredentialStore::new(store.clone(), leases, record.record.connection_id.clone());
    Ok((store, record, adapter))
}
