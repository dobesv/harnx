use super::{fixture::*, *};

#[tokio::test]
async fn stale_publication_cannot_overwrite_reconnect_after_credential_check() {
    assert_stale_publication_fenced(false).await;
}

#[tokio::test]
async fn stale_first_publication_cannot_overwrite_new_registration() {
    assert_stale_publication_fenced(true).await;
}

async fn assert_stale_publication_fenced(missing: bool) {
    let fixture = Fixture::new().await;
    let lifecycle = Arc::new(fixture.lifecycle().await);
    let old = connect(&lifecycle, &alice(), "old").await;
    if missing {
        let stored = registration(&lifecycle, &alice()).await;
        lifecycle
            .handle
            .delete(&Lifecycle::audience(&alice()), stored.revision, &old)
            .await
            .unwrap();
    }
    let (wait, resume) = lifecycle.hooks.arm(Point::PublicationChecked);
    let writer = lifecycle.clone();
    let stale = tokio::spawn(async move {
        writer
            .publish_registration(&alice(), &old, &tools("stale"), true)
            .await
    });
    reached(wait).await;
    let new = connect(&lifecycle, &alice(), "new").await;
    let before = registration(&lifecycle, &alice()).await;
    resume.send(()).unwrap();
    assert_eq!(stale.await.unwrap(), Err(Superseded));
    let after = registration(&lifecycle, &alice()).await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.registration.tools[0].name, "new");
    assert_refreshes(&lifecycle, &alice(), &new).await;
}

#[tokio::test]
async fn disconnect_cleanup_does_not_forget_a_concurrent_reconnect() {
    let fixture = Fixture::new().await;
    let lifecycle = Arc::new(fixture.lifecycle().await);
    connect(&lifecycle, &alice(), "old").await;
    let (wait, resume) = lifecycle.hooks.arm(Point::CredentialDeleted);
    let writer = lifecycle.clone();
    let disconnect = tokio::spawn(async move { writer.disconnect(&alice()).await });
    reached(wait).await;
    let new = connect(&lifecycle, &alice(), "new").await;
    resume.send(()).unwrap();
    assert!(matches!(
        disconnect.await.unwrap().unwrap(),
        DisconnectOutcome::Disconnected { .. }
    ));
    assert_eq!(lifecycle.managed.lock().await[&alice()].connection_id, new);
    assert_eq!(
        registration(&lifecycle, &alice()).await.registration.tools[0].name,
        "new"
    );
    assert_refreshes(&lifecycle, &alice(), &new).await;
}

#[tokio::test]
async fn catalog_cas_exhaustion_does_not_publish_registration() {
    let fixture = Fixture::new().await;
    let lifecycle = Arc::new(fixture.lifecycle().await);
    let connection_id = connect(&lifecycle, &alice(), "original").await;
    let before = registration(&lifecycle, &alice()).await;
    let pauses: Vec<_> = (0..CAS_ROUNDS)
        .map(|_| lifecycle.hooks.arm(Point::CatalogSnapshot))
        .collect();
    let writer = lifecycle.clone();
    let connection = connection_id.clone();
    let update = tokio::spawn(async move {
        writer
            .republish_catalog(&alice(), &connection, tools("uncommitted"), true)
            .await
    });
    let catalogs = lifecycle.catalogs.for_principal(alice()).unwrap();
    for (wait, resume) in pauses {
        reached(wait).await;
        let snapshot = catalogs.inspect().await.unwrap();
        catalogs
            .update(snapshot.revision, &connection_id, tools("winner"))
            .await
            .unwrap();
        resume.send(()).unwrap();
    }
    assert_eq!(
        update.await.unwrap().unwrap_err().to_string(),
        "catalog write is contended; try again"
    );
    let after = registration(&lifecycle, &alice()).await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.registration.tools[0].name, "original");
    lifecycle
        .republish_catalog(&alice(), &connection_id, tools("recovered"), true)
        .await
        .unwrap();
    assert_eq!(
        registration(&lifecycle, &alice()).await.registration.tools[0].name,
        "recovered"
    );
}

#[tokio::test]
async fn delayed_publication_bookkeeping_cannot_replace_newer_ownership() {
    let fixture = Fixture::new().await;
    let lifecycle = fixture.lifecycle().await;
    let old = connect(&lifecycle, &alice(), "old").await;
    let old_revision = registration(&lifecycle, &alice()).await.revision;
    let new = connect(&lifecycle, &alice(), "new").await;
    lifecycle
        .track_registration(&alice(), &old, old_revision)
        .await;
    assert_eq!(lifecycle.managed.lock().await[&alice()].connection_id, new);
    assert_refreshes(&lifecycle, &alice(), &new).await;
}
