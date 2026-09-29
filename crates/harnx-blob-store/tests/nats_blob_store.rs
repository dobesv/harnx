use anyhow::{Context, Result};
use harnx_blob_store::{
    delete_owner, ensure_plans_bucket,
    media::{ensure_attachments_bucket, get_media, put_media},
    media_cid_url, resolve, touch_activity, ResolvedBlob,
};
use harnx_core::cid_url::{CidUrl, PlanItem, SessionRef};

const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HASH_B: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

async fn isolated_jetstream() -> Result<
    Option<(
        harnx_test_bins::NatsServerHandle,
        async_nats::jetstream::Context,
    )>,
> {
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::connect(server.url())
        .await
        .context("connect to NATS test server")?;
    Ok(Some((server, async_nats::jetstream::new(client))))
}

#[tokio::test(flavor = "multi_thread")]
async fn media_put_get_round_trip() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(Some("pantheon/atlas".to_string()), "media1".to_string())?;
    let url = media_cid_url(&session, HASH_A);

    put_media(&store, &url, b"attachment bytes", "image/png").await?;

    assert_eq!(
        get_media(&store, &url).await?,
        Some((b"attachment bytes".to_vec(), "image/png".to_string()))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_media_returns_bytes_and_cache_metadata() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(Some("pantheon/atlas".to_string()), "resolv1".to_string())?;
    let url = media_cid_url(&session, HASH_A);
    put_media(&store, &url, b"resolved bytes", "text/plain").await?;

    assert_eq!(
        resolve(&jetstream, &url).await?,
        ResolvedBlob {
            mime_type: "text/plain".to_string(),
            bytes: b"resolved bytes".to_vec(),
            etag: Some(HASH_A.to_string()),
            immutable: true,
        }
    );
    let activity = jetstream.get_key_value("harnx_sessions").await?;
    let activity_key = format!("sessions/{}/activity", session.owner());
    assert!(activity.get(&activity_key).await?.is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_missing_media_returns_error() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(None, "miss01".to_string())?;
    let url = media_cid_url(&session, HASH_B);

    let error = resolve(&jetstream, &url).await.unwrap_err();
    assert!(error.to_string().contains("attachment not found"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_rejects_plan_urls() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let session = SessionRef::new(None, "plan01".to_string())?;
    let plan = CidUrl::Plan {
        session,
        slug: "project-plan".to_string(),
        item: PlanItem::Index,
    };

    let error = resolve(&jetstream, &plan).await.unwrap_err();
    assert_eq!(error.to_string(), "plan URLs not yet supported");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_owner_removes_media_and_plan_prefixes_only() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let media_store = ensure_attachments_bucket(&jetstream, 1).await?;
    let plans_store = ensure_plans_bucket(&jetstream, 1).await?;
    let deleted_session =
        SessionRef::new(Some("pantheon/atlas".to_string()), "delete1".to_string())?;
    let retained_session =
        SessionRef::new(Some("pantheon/atlas".to_string()), "retain1".to_string())?;
    let deleted_media = media_cid_url(&deleted_session, HASH_A);
    let retained_media = media_cid_url(&retained_session, HASH_B);
    put_media(&media_store, &deleted_media, b"delete", "text/plain").await?;
    put_media(&media_store, &retained_media, b"retain", "text/plain").await?;

    let deleted_plan = CidUrl::Plan {
        session: deleted_session.clone(),
        slug: "project-plan".to_string(),
        item: PlanItem::Index,
    };
    let deleted_task = CidUrl::Plan {
        session: deleted_session.clone(),
        slug: "project-plan".to_string(),
        item: PlanItem::Task("task-1".to_string()),
    };
    let retained_plan = CidUrl::Plan {
        session: retained_session,
        slug: "other-plan".to_string(),
        item: PlanItem::Index,
    };
    plans_store
        .put(&deleted_plan.kv_key(), b"plan".as_slice().into())
        .await?;
    plans_store
        .put(&deleted_task.kv_key(), b"task".as_slice().into())
        .await?;
    plans_store
        .put(&retained_plan.kv_key(), b"retain".as_slice().into())
        .await?;

    assert_eq!(delete_owner(&jetstream, &deleted_session.owner()).await?, 3);
    assert_eq!(get_media(&media_store, &deleted_media).await?, None);
    assert!(plans_store.get(&deleted_plan.kv_key()).await?.is_none());
    assert!(plans_store.get(&deleted_task.kv_key()).await?.is_none());
    assert!(get_media(&media_store, &retained_media).await?.is_some());
    assert!(plans_store.get(&retained_plan.kv_key()).await?.is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn touch_activity_debounces_second_write() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let owner = SessionRef::new(None, "touch01".to_string())?.owner();
    let key = format!("sessions/{owner}/activity");

    let first = touch_activity(&jetstream, &owner).await?;
    assert!(first.wrote());
    let store = jetstream
        .get_key_value("harnx_sessions")
        .await
        .context("open session activity bucket")?;
    let first_revision = store
        .entry(&key)
        .await?
        .context("first activity entry")?
        .revision;

    let second = touch_activity(&jetstream, &owner).await?;
    assert!(!second.wrote());
    let second_revision = store
        .entry(&key)
        .await?
        .context("second activity entry")?
        .revision;
    assert_eq!(second_revision, first_revision);
    Ok(())
}
