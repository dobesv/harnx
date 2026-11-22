use super::{fixture::*, *};

#[tokio::test]
async fn transient_registration_read_error_keeps_principal_until_recovery() {
    let fixture = Fixture::new().await;
    let mut lifecycle = fixture.lifecycle().await;
    let connection = connect(&lifecycle, &alice(), "original").await;
    let broken = fixture.broken_registration_handle(&lifecycle).await;
    let handle = std::mem::replace(&mut lifecycle.handle, broken);
    assert!(lifecycle.refresh_tick().await.is_err());
    assert_eq!(
        lifecycle.managed.lock().await[&alice()].connection_id,
        connection
    );
    lifecycle.handle = handle;
    assert_refreshes(&lifecycle, &alice(), &connection).await;
}

#[tokio::test]
async fn transient_credential_read_error_keeps_principal_until_recovery() {
    let fixture = Fixture::new().await;
    let lifecycle = fixture.lifecycle().await;
    let connection = connect(&lifecycle, &alice(), "original").await;
    let bucket = harnx_credentials::store::CREDENTIALS_BUCKET;
    let kv = fixture.js.get_key_value(bucket).await.unwrap();
    let key = lifecycle.credentials.for_principal(alice()).unwrap().key();
    let encrypted = kv.get(&key).await.unwrap().unwrap();
    fixture.js.delete_key_value(bucket).await.unwrap();
    assert!(lifecycle.refresh_tick().await.is_err());
    assert_eq!(
        lifecycle.managed.lock().await[&alice()].connection_id,
        connection
    );
    let restored = fixture
        .js
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: bucket.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    restored.put(&key, encrypted).await.unwrap();
    assert_refreshes(&lifecycle, &alice(), &connection).await;
}

#[tokio::test]
async fn update_error_does_not_abort_remaining_principals_and_retries() {
    let fixture = Fixture::new().await;
    let lifecycle = fixture.lifecycle().await;
    let bob = Principal::User("bob".into());
    connect(&lifecycle, &alice(), "alice").await;
    connect(&lifecycle, &bob, "bob").await;
    // Pick the first map entry so an early return would starve the other entry.
    let first = lifecycle
        .managed
        .lock()
        .await
        .keys()
        .next()
        .unwrap()
        .clone();
    let other = if first == alice() { bob } else { alice() };
    let stored = registration(&lifecycle, &first).await;
    let mut large = stored.registration;
    large.tools[0].description = "description".repeat(8192);
    let failed_before = lifecycle
        .handle
        .update(&Lifecycle::audience(&first), large, stored.revision)
        .await
        .unwrap();
    let other_before = registration(&lifecycle, &other).await.revision;
    let mut stream = fixture
        .js
        .get_stream(format!("KV_{}", harnx_toolset_server::TOOL_REGISTRY_BUCKET))
        .await
        .unwrap();
    let original = stream.info().await.unwrap().config.clone();
    let mut restricted = original.clone();
    // The first principal's refresh fails at the broker; the other still fits.
    restricted.max_message_size = 4096;
    fixture.js.update_stream(restricted).await.unwrap();
    assert!(lifecycle.refresh_tick().await.is_err());
    assert_eq!(
        registration(&lifecycle, &first).await.revision,
        failed_before
    );
    assert!(registration(&lifecycle, &other).await.revision > other_before);
    assert_eq!(lifecycle.managed.lock().await.len(), 2);
    fixture.js.update_stream(original).await.unwrap();
    lifecycle.refresh_tick().await.unwrap();
    assert!(registration(&lifecycle, &first).await.revision > failed_before);
}

#[tokio::test]
async fn stale_refresh_snapshot_does_not_forget_a_concurrent_reconnect() {
    let fixture = Fixture::new().await;
    let lifecycle = Arc::new(fixture.lifecycle().await);
    connect(&lifecycle, &alice(), "old").await;
    let (wait, resume) = lifecycle.hooks.arm(Point::RefreshSnapshot);
    let writer = lifecycle.clone();
    let tick = tokio::spawn(async move { writer.refresh_tick().await });
    reached(wait).await;
    let new = connect(&lifecycle, &alice(), "new").await;
    resume.send(()).unwrap();
    tick.await.unwrap().unwrap();
    assert_eq!(lifecycle.managed.lock().await[&alice()].connection_id, new);
    assert_refreshes(&lifecycle, &alice(), &new).await;
}

#[tokio::test]
async fn survivor_adopts_a_crashed_instances_registration_without_upstream_calls() {
    let fixture = Fixture::new().await;
    // B is already running before A establishes this connection.
    let survivor = fixture.lifecycle().await;
    survivor.recover_at_startup().await.unwrap();
    let crashed = fixture.lifecycle().await;
    let connection = connect(&crashed, &alice(), "original").await;
    drop(crashed);
    assert!(survivor.managed.lock().await.is_empty());
    survivor.refresh_tick().await.unwrap();
    assert_eq!(
        survivor.managed.lock().await[&alice()].connection_id,
        connection
    );
    assert_refreshes(&survivor, &alice(), &connection).await;
    assert_eq!(
        registration(&survivor, &alice()).await.registration.tools[0].name,
        "original"
    );
}

#[tokio::test]
async fn takeover_does_not_adopt_mismatched_or_deleted_registrations() {
    let fixture = Fixture::new().await;
    let owner = fixture.lifecycle().await;
    let survivor = fixture.lifecycle().await;
    let old = connect(&owner, &alice(), "old").await;
    let current = connect(&owner, &alice(), "new").await;
    let stored = registration(&owner, &alice()).await;
    let mut payload = stored.registration;
    payload.connection_id = Some(old);
    owner
        .handle
        .update(
            &Lifecycle::audience(&alice()),
            payload.clone(),
            stored.revision,
        )
        .await
        .unwrap();
    survivor.refresh_tick().await.unwrap();
    assert!(survivor.managed.lock().await.is_empty());
    let stored = registration(&owner, &alice()).await;
    payload.connection_id = Some(current.clone());
    owner
        .handle
        .update(&Lifecycle::audience(&alice()), payload, stored.revision)
        .await
        .unwrap();
    owner.disconnect(&alice()).await.unwrap();
    survivor.refresh_tick().await.unwrap();
    assert!(survivor.managed.lock().await.is_empty());
    assert!(survivor
        .handle
        .read(&Lifecycle::audience(&alice()))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn startup_catalog_read_failure_does_not_prevent_other_principals_recovery() {
    let fixture = Fixture::new().await;
    let owner = fixture.lifecycle().await;
    let bob = Principal::User("bob".into());
    connect(&owner, &alice(), "alice").await;
    connect(&owner, &bob, "bob").await;
    let principals = owner.credentials.list_active_principals().await.unwrap();
    let unreadable = &principals[0];
    let other = &principals[1];
    let catalogs = owner.catalogs.for_principal(unreadable.clone()).unwrap();
    let kv = fixture
        .js
        .get_key_value(harnx_credentials::catalog::CATALOGS_BUCKET)
        .await
        .unwrap();
    let original = kv.get(catalogs.key()).await.unwrap().unwrap();
    kv.put(catalogs.key(), "invalid catalog".into())
        .await
        .unwrap();
    let stored = registration(&owner, other).await;
    owner
        .handle
        .delete(
            &Lifecycle::audience(other),
            stored.revision,
            stored.registration.connection_id.as_deref().unwrap(),
        )
        .await
        .unwrap();
    let survivor = fixture.lifecycle().await;
    assert!(survivor.recover_at_startup().await.is_err());
    assert!(registration(&survivor, other).await.revision > stored.revision);
    assert!(survivor.managed.lock().await.contains_key(other));
    kv.put(catalogs.key(), original).await.unwrap();
    survivor.recover_at_startup().await.unwrap();
    assert_eq!(survivor.managed.lock().await.len(), 2);
}
