use anyhow::Result;
use harnx_credentials::{Catalog, CatalogStore, Error};

use crate::common::*;

#[tokio::test]
async fn catalog_cas_and_connection_fencing_reject_old_client() -> Result<()> {
    let (_server, js) = nats().await?;
    let credentials = credentials(&js).await?;
    let connected = credentials.put_connect(0, connect("secret-marker")).await?;
    let id = &connected.record.connection_id;
    let catalogs = CatalogStore::open(&js, 1, "jira", credentials.principal().clone()).await?;
    assert_eq!(catalogs.key(), "catalog.v1.amlyYQ.YWxpY2U");
    let catalog = catalogs
        .publish(
            &credentials,
            0,
            Catalog {
                connection_id: id.clone(),
                tools: vec![tool("first")],
            },
        )
        .await?;
    assert_eq!(
        catalogs
            .update(catalog.revision, "wrong-id", vec![])
            .await
            .unwrap_err(),
        Error::ConnectionMismatch
    );
    let updated = catalogs
        .update(catalog.revision, id, vec![tool("second")])
        .await?;
    assert_eq!(
        catalogs
            .update(catalog.revision, id, vec![tool("stale")])
            .await
            .unwrap_err(),
        Error::Conflict
    );
    assert!(!catalogs.delete_if_connection("wrong-id").await?);
    assert_eq!(
        catalogs.get().await?.unwrap().record.tools[0].name,
        "second"
    );
    let reconnect = credentials
        .put_connect(connected.revision, connect("new-secret-marker"))
        .await?;
    let new_id = &reconnect.record.connection_id;
    let newer = catalogs
        .publish(
            &credentials,
            updated.revision,
            Catalog {
                connection_id: new_id.clone(),
                tools: vec![tool("new-account")],
            },
        )
        .await?;
    assert!(!catalogs.delete_if_connection(id).await?);
    assert_eq!(
        catalogs
            .update(newer.revision, id, vec![])
            .await
            .unwrap_err(),
        Error::ConnectionMismatch
    );
    assert_eq!(
        catalogs
            .publish(
                &credentials,
                newer.revision,
                Catalog {
                    connection_id: id.clone(),
                    tools: vec![]
                }
            )
            .await
            .unwrap_err(),
        Error::ConnectionMismatch
    );
    assert!(catalogs.delete_if_connection(new_id).await?);
    assert!(catalogs.get().await?.is_none());
    assert_eq!(
        catalogs
            .publish(
                &credentials,
                0,
                Catalog {
                    connection_id: new_id.clone(),
                    tools: vec![]
                }
            )
            .await
            .unwrap_err(),
        Error::Conflict
    );
    Ok(())
}

#[tokio::test]
async fn expired_needs_login_and_cross_principal_cannot_publish_catalog() -> Result<()> {
    let (_server, js) = nats().await?;
    let credentials = credentials(&js).await?;
    let connected = credentials.put_connect(0, connect("secret-marker")).await?;
    let catalogs = CatalogStore::open(&js, 1, "jira", credentials.principal().clone()).await?;
    let catalog = Catalog {
        connection_id: connected.record.connection_id.clone(),
        tools: vec![],
    };
    let bob = credentials.for_principal(harnx_credentials::Principal::User("bob".into()))?;
    assert_eq!(
        catalogs
            .publish(&bob, 0, catalog.clone())
            .await
            .unwrap_err(),
        Error::ConnectionMismatch
    );
    credentials.mark_needs_login(connected.revision).await?;
    assert_eq!(
        catalogs
            .publish(&credentials, 0, catalog)
            .await
            .unwrap_err(),
        Error::NeedsLogin
    );
    Ok(())
}
