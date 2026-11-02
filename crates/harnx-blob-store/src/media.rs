//! Media object store operations on the `harnx_attachments` bucket.

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, object_store, stream};
use tokio::io::AsyncReadExt;

use harnx_core::cid_url::CidUrl;

/// JetStream object-store bucket containing durable session attachment blobs.
pub const ATTACHMENTS_BUCKET: &str = "harnx_attachments";

/// Content type metadata key for the object store.
const CONTENT_TYPE_METADATA_KEY: &str = "content_type";

/// Put a media blob into the object store.
///
/// The blob is stored at the key specified by `CidUrl::kv_key()`.
pub async fn put_media(
    store: &object_store::ObjectStore,
    url: &CidUrl,
    bytes: &[u8],
    mime_type: &str,
) -> Result<()> {
    put_media_chunked(
        store,
        MediaBlob {
            url,
            bytes,
            mime_type,
        },
        None,
    )
    .await
}

/// A media object's identity, exact bytes and stored content type.
#[derive(Clone, Copy)]
pub struct MediaBlob<'a> {
    pub url: &'a CidUrl,
    pub bytes: &'a [u8],
    pub mime_type: &'a str,
}

/// Put a blob with chunks no larger than `max_chunk_bytes`. Use the connected
/// broker's payload ceiling when it is smaller than the client's 128 KiB default.
/// Object metadata still has to fit in a single message; failures propagate.
pub async fn put_media_with_chunk_limit(
    store: &object_store::ObjectStore,
    blob: MediaBlob<'_>,
    max_chunk_bytes: usize,
) -> Result<()> {
    anyhow::ensure!(
        max_chunk_bytes > 0,
        "attachment chunk limit must be positive"
    );
    put_media_chunked(store, blob, Some(max_chunk_bytes.min(128 * 1024))).await
}

async fn put_media_chunked(
    store: &object_store::ObjectStore,
    blob: MediaBlob<'_>,
    chunk_size: Option<usize>,
) -> Result<()> {
    let MediaBlob {
        url,
        bytes,
        mime_type,
    } = blob;
    let key = url.kv_key();
    if store.info(&key).await.is_ok() {
        return Ok(()); // Already exists, skip duplicate put
    }
    let mut metadata = HashMap::new();
    metadata.insert(CONTENT_TYPE_METADATA_KEY.to_string(), mime_type.to_string());
    let object = object_store::ObjectMetadata {
        name: key.clone(),
        description: Some(format!("Attachment {}", url)),
        metadata,
        chunk_size,
        ..Default::default()
    };
    let mut cursor = std::io::Cursor::new(bytes);
    store
        .put(object, &mut cursor)
        .await
        .with_context(|| format!("failed to put media blob '{}'", key))?;

    Ok(())
}

/// Get a media blob from the object store.
///
/// Returns the bytes and the MIME type, or None if not found.
pub async fn get_media(
    store: &object_store::ObjectStore,
    url: &CidUrl,
) -> Result<Option<(Vec<u8>, String)>> {
    get_media_with_limit(store, url, None).await
}

/// Read at most `max_bytes` of a media object. Check metadata before download and
/// bound the stream too, so a stale or dishonest size cannot cause an unbounded
/// allocation. Oversized objects fail without returning a truncated payload.
pub async fn get_media_bounded(
    store: &object_store::ObjectStore,
    url: &CidUrl,
    max_bytes: u64,
) -> Result<Option<(Vec<u8>, String)>> {
    let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    get_media_with_limit(store, url, Some(max_bytes)).await
}

pub(crate) async fn get_media_with_limit(
    store: &object_store::ObjectStore,
    url: &CidUrl,
    max_bytes: Option<usize>,
) -> Result<Option<(Vec<u8>, String)>> {
    let key = url.kv_key();

    let object = match store.get(&key).await {
        Ok(object) => object,
        Err(error) if error.kind() == object_store::GetErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::Error::from(error))
                .with_context(|| format!("download media blob '{}'", key));
        }
    };

    // ObjectStore::get only fetches metadata; chunk download starts on the first read.
    crate::limits::check_size(object.info().size, max_bytes)?;
    let mime_type = object
        .info()
        .metadata
        .get(CONTENT_TYPE_METADATA_KEY)
        .cloned()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let bytes = read_media_bytes(object, &key, max_bytes).await?;
    Ok(Some((bytes, mime_type)))
}

async fn read_media_bytes(
    object: object_store::Object,
    key: &str,
    max_bytes: Option<usize>,
) -> Result<Vec<u8>> {
    let read_limit = max_bytes
        .map(|max| (max as u64).saturating_add(1))
        .unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    object
        .take(read_limit)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("read media blob '{}'", key))?;
    // Metadata can understate the size. Never trust it as the only bound.
    crate::limits::check_size(bytes.len(), max_bytes)?;
    Ok(bytes)
}

fn object_store_stream_name() -> String {
    format!("OBJ_{}", ATTACHMENTS_BUCKET)
}

fn stream_missing(kind: &jetstream::context::GetStreamErrorKind) -> bool {
    matches!(
        kind,
        jetstream::context::GetStreamErrorKind::JetStream(error)
            if error.kind() == jetstream::ErrorCode::STREAM_NOT_FOUND
    )
}

async fn raise_object_store_replicas(
    jetstream: &jetstream::Context,
    replicas: usize,
) -> Result<()> {
    let stream_name = object_store_stream_name();
    let mut stream = jetstream
        .get_stream(&stream_name)
        .await
        .with_context(|| format!("get attachment object-store stream '{stream_name}'"))?;
    let mut config = stream
        .info()
        .await
        .with_context(|| format!("read attachment object-store stream '{stream_name}'"))?
        .config
        .clone();
    if replicas <= config.num_replicas {
        return Ok(());
    }
    config.num_replicas = replicas;
    jetstream
        .update_stream(config)
        .await
        .with_context(|| format!("raise attachment object-store replicas to {replicas}"))?;
    Ok(())
}

/// Ensure the attachments bucket exists.
pub async fn ensure_attachments_bucket(
    jetstream: &jetstream::Context,
    replicas: usize,
) -> Result<object_store::ObjectStore> {
    let create = jetstream
        .create_object_store(object_store::Config {
            bucket: ATTACHMENTS_BUCKET.to_string(),
            description: Some("Harnx session attachment blobs".to_string()),
            storage: stream::StorageType::File,
            num_replicas: replicas,
            ..Default::default()
        })
        .await;
    if let Ok(store) = create {
        return Ok(store);
    }
    if let Err(error) = raise_object_store_replicas(jetstream, replicas).await {
        log::warn!(
            "could not reconcile replicas for attachment object store '{ATTACHMENTS_BUCKET}': {error:#}"
        );
    }
    jetstream
        .get_object_store(ATTACHMENTS_BUCKET)
        .await
        .map_err(anyhow::Error::from)
        .context("open NATS session attachment object store")
}

/// Get an existing attachments bucket, or None if it doesn't exist.
pub async fn optional_attachments_bucket(
    jetstream: &jetstream::Context,
) -> Result<Option<object_store::ObjectStore>> {
    match jetstream.get_stream(object_store_stream_name()).await {
        Ok(_) => jetstream
            .get_object_store(ATTACHMENTS_BUCKET)
            .await
            .map(Some)
            .map_err(anyhow::Error::from)
            .context("open NATS session attachment object store"),
        Err(error) if stream_missing(&error.kind()) => Ok(None),
        Err(error) => {
            Err(anyhow::Error::from(error)).context("inspect NATS session attachment object store")
        }
    }
}

/// Delete all media for an owner (prefix: `media/<owner>/`).
pub async fn delete_media_prefix(jetstream: &jetstream::Context, owner: &str) -> Result<usize> {
    let Some(store) = optional_attachments_bucket(jetstream).await? else {
        return Ok(0);
    };

    let prefix = format!("media/{}", owner);
    let mut objects = store.list().await.context("list media objects")?;
    let mut names = Vec::new();

    use futures_util::StreamExt;
    while let Some(info) = objects.next().await {
        let info = info.context("list media object metadata")?;
        if info.name.starts_with(&prefix) || info.name.starts_with(&format!("{}/", prefix)) {
            names.push(info.name);
        }
    }

    for name in &names {
        store
            .delete(name)
            .await
            .with_context(|| format!("delete media object '{}'", name))?;
    }

    Ok(names.len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn kv_key_format() {
        use harnx_core::cid_url::SessionRef;
        let session = SessionRef::new(None, "abcDEF".to_string()).unwrap();
        let url = crate::media_cid_url(
            &session,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );
        assert!(url.kv_key().starts_with("media/"));
        assert!(url
            .kv_key()
            .ends_with("/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));
    }
}
