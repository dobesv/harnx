use anyhow::Result;
use chrono::{TimeDelta, Utc};
use harnx_credentials::{
    envelope,
    flow::{challenge_key, state_key, FlowOutcome, FLOWS_BUCKET},
    Error, FlowOptions, FlowStatus, PendingFlowStore, Principal, FLOW_TTL,
};

use crate::common::*;

#[tokio::test]
async fn second_instance_loads_original_oauth_and_full_client_registration() -> Result<()> {
    let (_server, js) = nats().await?;
    let first = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let original = first
        .create(Principal::User("alice".into()), flow_options())
        .await?;
    let second = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let challenge = original.record.challenge_id.expose();
    let from_challenge = second.get_by_challenge(challenge).await?.unwrap();
    let from_state = second
        .get_by_state(original.record.oauth_state.expose())
        .await?
        .unwrap();
    assert_eq!(from_state.revision, original.revision);
    assert_eq!(
        from_challenge.record.oauth_state.expose(),
        original.record.oauth_state.expose()
    );
    assert_eq!(
        from_state.record.pkce_verifier.as_ref().unwrap().expose(),
        "pkce-secret-marker"
    );
    let client = from_state.record.oauth_client.unwrap();
    assert_eq!(
        client.client_secret.unwrap().expose(),
        "client-secret-marker"
    );
    assert_eq!(
        client.registration.unwrap().expose().additional_fields["registration_access_token"],
        "registration-secret-marker"
    );
    assert_eq!(
        from_state
            .record
            .authorization_state
            .unwrap()
            .expose()
            .expected_issuer
            .as_deref(),
        Some("https://issuer.test")
    );
    let kv = js.get_key_value(FLOWS_BUCKET).await?;
    let config = kv.stream.cached_info().config.clone();
    assert_eq!(config.max_age, FLOW_TTL);
    let index = kv
        .get(state_key(original.record.oauth_state.expose()))
        .await?
        .unwrap();
    assert_eq!(index.as_ref(), challenge_key(challenge).as_bytes());
    let bytes = kv.get(challenge_key(challenge)).await?.unwrap();
    for secret in [
        challenge,
        original.record.oauth_state.expose(),
        "pkce-secret-marker",
        "client-secret-marker",
    ] {
        assert!(!String::from_utf8_lossy(&bytes).contains(secret));
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_claim_has_exactly_one_winner_and_completed_cancel_is_noop() -> Result<()> {
    let (_server, js) = nats().await?;
    let first = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let second = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let flow = first
        .create(Principal::Ownerless, FlowOptions::default())
        .await?;
    let challenge = flow.record.challenge_id.expose();
    assert_ne!(challenge, flow.record.oauth_state.expose());
    let (a, b) = tokio::join!(
        first.claim(challenge, flow.revision),
        second.claim(challenge, flow.revision)
    );
    let winner = one_winner(a, b, Error::Conflict);
    let complete = second
        .complete(challenge, winner.revision, "connection-1".into())
        .await?;
    let cancelled = first.cancel(challenge, flow.revision).await?;
    assert_eq!(cancelled.record.status, FlowStatus::Completed);
    assert_eq!(cancelled.revision, complete.revision);
    assert!(
        matches!(cancelled.record.outcome, Some(FlowOutcome::Connected { connection_id }) if connection_id == "connection-1")
    );
    assert_eq!(
        first.claim(challenge, complete.revision).await.unwrap_err(),
        Error::InvalidTransition
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_and_failure_are_terminal_and_revision_checked() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let flow = store
        .create(Principal::Ownerless, FlowOptions::default())
        .await?;
    let challenge = flow.record.challenge_id.expose();
    assert_eq!(
        store.claim(challenge, flow.revision + 1).await.unwrap_err(),
        Error::Conflict
    );
    let cancelled = store.cancel(challenge, flow.revision).await?;
    assert_eq!(cancelled.record.status, FlowStatus::Cancelled);
    assert_eq!(
        store
            .claim(challenge, cancelled.revision)
            .await
            .unwrap_err(),
        Error::InvalidTransition
    );
    let flow = store
        .create(Principal::Ownerless, FlowOptions::default())
        .await?;
    let challenge = flow.record.challenge_id.expose();
    let claimed = store.claim(challenge, flow.revision).await?;
    assert_eq!(
        store.cancel(challenge, flow.revision).await?.record.status,
        FlowStatus::Claimed
    );
    let failed = store
        .fail(challenge, claimed.revision, "safe error".into())
        .await?;
    assert_eq!(failed.record.status, FlowStatus::Failed);
    assert_eq!(
        store
            .complete(challenge, failed.revision, "conn".into())
            .await
            .unwrap_err(),
        Error::InvalidTransition
    );
    Ok(())
}

#[tokio::test]
async fn flow_absolute_expiry_blocks_claim_and_bad_state_index_is_rejected() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let original = store
        .create(Principal::Ownerless, FlowOptions::default())
        .await?;
    let challenge = original.record.challenge_id.expose().clone();
    let kv = js.get_key_value(FLOWS_BUCKET).await?;
    kv.put(state_key("forged-state"), challenge_key(&challenge).into())
        .await?;
    assert_eq!(
        store.get_by_state("forged-state").await.unwrap_err(),
        Error::AuthenticationFailed
    );
    let mut expired = original.record;
    expired.expires_at = Utc::now() - TimeDelta::seconds(1);
    let key = challenge_key(&challenge);
    let bytes = envelope::seal(keyring()?.as_ref(), FLOWS_BUCKET, &key, &expired)?;
    kv.update(&key, bytes.into(), original.revision).await?;
    assert_eq!(
        store.get_by_challenge(&challenge).await.unwrap_err(),
        Error::Expired
    );
    assert_eq!(
        store
            .get_by_state(expired.oauth_state.expose())
            .await
            .unwrap_err(),
        Error::Expired
    );
    assert_eq!(
        store
            .claim(&challenge, original.revision)
            .await
            .unwrap_err(),
        Error::Expired
    );
    Ok(())
}

#[tokio::test]
async fn failed_state_index_write_does_not_return_a_usable_flow() -> Result<()> {
    let (_server, js) = nats().await?;
    let store = PendingFlowStore::open(&js, 1, keyring()?, "jira").await?;
    let options = flow_options();
    let original = store.create(Principal::Ownerless, options.clone()).await?;
    // Same prepared OAuth state causes create-only index CAS to fail.
    assert_eq!(
        store
            .create(Principal::Ownerless, options)
            .await
            .unwrap_err(),
        Error::Conflict
    );
    assert_eq!(
        store
            .get_by_state(original.record.oauth_state.expose())
            .await?
            .unwrap()
            .revision,
        original.revision
    );
    Ok(())
}
