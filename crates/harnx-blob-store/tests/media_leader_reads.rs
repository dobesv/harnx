use anyhow::{Context, Result};
use async_nats::jetstream::{self, stream};
use base64::{engine::general_purpose, Engine as _};
use harnx_blob_store::{
    media::{
        delete_media_prefix, ensure_attachments_bucket, get_media, leader_media_info,
        media_content_type, purge_media_version, put_media, put_media_with_chunk_limit,
        read_media_from_leader, AttachmentsBucket, MediaBlob, ATTACHMENTS_BUCKET,
    },
    media_cid_url,
};
use harnx_core::{
    cid_url::{CidUrl, SessionRef},
    crypto::sha256,
    require_nextest,
};
use harnx_test_bins::{spawn_configured_nats_server, NatsServerHandle};

const MEDIA: &[u8] = b"some stored attachment bytes";
const MIME: &str = "text/plain";

async fn attachment_bucket(
    config: &str,
) -> Result<Option<(NatsServerHandle, jetstream::Context, AttachmentsBucket)>> {
    require_nextest();
    let Some(server) = spawn_configured_nats_server(config).await? else {
        return Ok(None);
    };
    let jetstream = jetstream::new(async_nats::connect(server.url()).await?);
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    Ok(Some((server, jetstream, store)))
}

/// The attachments object-store stream name, as harnx-blob-store builds it.
fn object_store_stream_name() -> String {
    format!("OBJ_{ATTACHMENTS_BUCKET}")
}

fn media_url(session: &SessionRef) -> CidUrl {
    let encoded = general_purpose::STANDARD.encode(MEDIA);
    media_cid_url(session, &sha256(&format!("data:{MIME};base64,{encoded}")))
}

#[tokio::test]
async fn leader_media_info_distinguishes_absent_live_and_deleted() -> Result<()> {
    let Some((_server, _, store)) = attachment_bucket("").await? else {
        return Ok(());
    };
    let url = media_url(&SessionRef::new(None, "leader".into())?);
    let key = url.kv_key();
    assert!(leader_media_info(&store, &key).await?.is_none());

    put_media(&store, &url, MEDIA, MIME).await?;
    let info = leader_media_info(&store, &key)
        .await?
        .context("live info")?;
    assert!(!info.deleted);
    assert_eq!(info.name, key);
    assert_eq!(info.size, MEDIA.len());
    assert_eq!(media_content_type(&info), MIME);

    put_media(&store, &url, MEDIA, MIME).await?;
    let repeated = leader_media_info(&store, &key)
        .await?
        .context("dedup info")?;
    assert_eq!(
        repeated.nuid, info.nuid,
        "a live object should not be rewritten"
    );

    store.delete(&key).await?;
    let tombstone = leader_media_info(&store, &key)
        .await?
        .context("tombstone info")?;
    assert!(tombstone.deleted);
    assert_eq!(tombstone.nuid, info.nuid);
    Ok(())
}

#[tokio::test]
async fn put_media_restores_a_tombstoned_cid() -> Result<()> {
    let Some((_server, _, store)) = attachment_bucket("").await? else {
        return Ok(());
    };
    let url = media_url(&SessionRef::new(None, "restore".into())?);
    put_media(&store, &url, MEDIA, MIME).await?;
    store.delete(url.kv_key()).await?;
    assert_eq!(get_media(&store, &url).await?, None);

    put_media(&store, &url, MEDIA, MIME).await?;
    assert_eq!(
        get_media(&store, &url).await?,
        Some((MEDIA.to_vec(), MIME.to_owned()))
    );
    Ok(())
}

#[tokio::test]
async fn put_media_writes_when_leader_dedup_probe_fails() -> Result<()> {
    // A missing target stream returns a JetStream error rather than an object
    // miss. Only raw gets are redirected; async-nats put uses a best-effort
    // direct get for old metadata and publishes chunks and metadata normally.
    let config = format!(
        r#"mappings = {{
  "$JS.API.STREAM.MSG.GET.OBJ_{ATTACHMENTS_BUCKET}": "$JS.API.STREAM.MSG.GET.MISSING_ATTACHMENTS"
}}"#
    );
    let Some((_server, _, store)) = attachment_bucket(&config).await? else {
        return Ok(());
    };
    let url = media_url(&SessionRef::new(None, "dedup-probe-failure".into())?);
    let error = leader_media_info(&store, &url.kv_key()).await.unwrap_err();
    let cause = error
        .downcast_ref::<stream::LastRawMessageError>()
        .context("expected a JetStream raw get error")?;
    assert!(matches!(
        cause.kind(),
        stream::LastRawMessageErrorKind::JetStream(error)
            if error.error_code() == jetstream::ErrorCode::STREAM_NOT_FOUND
    ));

    put_media(&store, &url, MEDIA, MIME).await?;
    assert_eq!(
        get_media(&store, &url).await?,
        Some((MEDIA.to_vec(), MIME.to_owned()))
    );
    Ok(())
}

const STALE_REPLICA: &str = "STALE_ATTACHMENTS_REPLICA";
const STALE_META: &str = "harnx.test.stale-meta";

fn stale_replica_config() -> String {
    // Map the suffix used by async-nats 0.51's direct metadata get. Store the
    // stale copy under a separate subject so it doesn't also reach the leader.
    format!(
        r#"mappings = {{
  "$JS.API.DIRECT.GET.OBJ_{ATTACHMENTS_BUCKET}.$O.{ATTACHMENTS_BUCKET}.M.*": "$JS.API.DIRECT.GET.{STALE_REPLICA}.{STALE_META}.{{{{wildcard(1)}}}}"
}}"#
    )
}

async fn save_stale_metadata(jetstream: &jetstream::Context, key: &str) -> Result<()> {
    jetstream
        .create_stream(stream::Config {
            name: STALE_REPLICA.into(),
            subjects: vec![format!("{STALE_META}.*")],
            allow_direct: true,
            storage: stream::StorageType::Memory,
            ..Default::default()
        })
        .await?;
    let stream = jetstream
        .get_stream_no_info(object_store_stream_name())
        .await?;
    let encoded_key = general_purpose::URL_SAFE.encode(key);
    let metadata = stream
        .get_last_raw_message_by_subject(&format!("$O.{ATTACHMENTS_BUCKET}.M.{encoded_key}"))
        .await?;
    jetstream
        .publish(format!("{STALE_META}.{encoded_key}"), metadata.payload)
        .await?
        .await?;
    Ok(())
}

#[tokio::test]
async fn put_media_ignores_stale_live_metadata_after_purge() -> Result<()> {
    let Some((_server, jetstream, store)) = attachment_bucket(&stale_replica_config()).await?
    else {
        return Ok(());
    };
    let session = SessionRef::new(None, "purged".into())?;
    let url = media_url(&session);
    let key = url.kv_key();
    put_media(&store, &url, MEDIA, MIME).await?;
    save_stale_metadata(&jetstream, &key).await?;
    assert_eq!(delete_media_prefix(&jetstream, &session.owner()).await?, 1);

    let stale = store
        .info(&key)
        .await
        .context("direct get should still report the purged object")?;
    assert!(!stale.deleted);
    assert!(leader_media_info(&store, &key).await?.is_none());

    put_media(&store, &url, MEDIA, MIME).await?;
    let fresh = leader_media_info(&store, &key)
        .await?
        .context("re-put did not write to the leader")?;
    assert!(!fresh.deleted);
    assert_ne!(fresh.nuid, stale.nuid);
    assert_eq!(
        store.info(&key).await?.nuid,
        stale.nuid,
        "direct reads still lag"
    );
    assert_eq!(read_media_from_leader(&store, &fresh).await?, MEDIA);
    assert_eq!(media_content_type(&fresh), MIME);
    Ok(())
}

#[tokio::test]
async fn read_media_from_leader_reads_all_chunks_and_rejects_shortfalls() -> Result<()> {
    let Some((_server, jetstream, store)) = attachment_bucket("").await? else {
        return Ok(());
    };
    let url = media_url(&SessionRef::new(None, "chunks".into())?);
    let blob = MediaBlob {
        url: &url,
        bytes: MEDIA,
        mime_type: MIME,
    };
    put_media_with_chunk_limit(&store, blob, 8).await?;
    let mut info = leader_media_info(&store, &url.kv_key())
        .await?
        .context("chunked object info")?;
    assert!(info.chunks > 1);
    assert_eq!(read_media_from_leader(&store, &info).await?, MEDIA);

    info.size += 1;
    assert!(
        read_media_from_leader(&store, &info).await.is_err(),
        "byte count is short despite reading all chunks"
    );
    info.size -= 1;

    let stream = jetstream
        .get_stream_no_info(object_store_stream_name())
        .await?;
    let subject = format!("$O.{ATTACHMENTS_BUCKET}.C.{}", info.nuid);
    let first = stream.get_first_raw_message_by_subject(&subject, 0).await?;
    assert!(stream.delete_message(first.sequence).await?);
    let error = read_media_from_leader(&store, &info).await.unwrap_err();
    let cause = error
        .downcast_ref::<stream::RawMessageError>()
        .context("expected a missing chunk error")?;
    assert_eq!(cause.kind(), stream::RawMessageErrorKind::NoMessageFound);
    Ok(())
}

#[tokio::test]
async fn purge_media_version_keeps_the_object_whole_when_the_metadata_delete_fails() -> Result<()> {
    let Some((_server, jetstream, store)) = attachment_bucket("").await? else {
        return Ok(());
    };
    let url = media_url(&SessionRef::new(None, "purgefail".into())?);
    put_media(&store, &url, MEDIA, MIME).await?;
    let info = leader_media_info(&store, &url.kv_key())
        .await?
        .context("written object info")?;
    let nuid = info.nuid.clone();

    // `deny_delete` rejects the metadata message delete — the purge's
    // first step — while leaving chunk purges allowed, so the assertions
    // below actually detect the ordering: a chunk-purge-first
    // implementation would succeed at its first step and then strand live
    // metadata pointing at purged chunks. (A sealed store, which the test
    // used before, rejects both operations and detects nothing.)
    let mut config = jetstream
        .get_stream(object_store_stream_name())
        .await?
        .info()
        .await?
        .config
        .clone();
    config.deny_delete = true;
    jetstream.update_stream(config).await?;

    assert!(
        purge_media_version(&store, &url, &nuid).await.is_err(),
        "a delete-denying stream must fail the metadata delete"
    );

    // Partial failure must leave the object whole: metadata still names the
    // version and the chunks read back. The chunk-purge-first order would
    // have left live metadata pointing at purged chunks.
    let info = leader_media_info(&store, &url.kv_key())
        .await?
        .context("metadata must survive a failed purge")?;
    assert_eq!(info.nuid, nuid);
    assert_eq!(read_media_from_leader(&store, &info).await?, MEDIA);
    Ok(())
}
