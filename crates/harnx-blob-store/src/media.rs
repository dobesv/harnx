//! Media object store operations on the `harnx_attachments` bucket.

use std::{collections::HashMap, ops::Deref};

use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, object_store, stream};
use base64::Engine as _;
use tokio::io::AsyncReadExt;

use harnx_core::cid_url::CidUrl;
use harnx_nats_common::leader_reads::retry_transient;

/// JetStream object-store bucket containing durable session attachment blobs.
pub const ATTACHMENTS_BUCKET: &str = "harnx_attachments";

/// Content type metadata key for the object store.
const CONTENT_TYPE_METADATA_KEY: &str = "content_type";

/// The attachment object store and a handle for reading its stream leader.
///
/// `ObjectStore` doesn't expose its stream or context. Keep both handles so
/// writes and leader reads always use the same JetStream account and domain.
#[derive(Clone)]
pub struct AttachmentsBucket {
    store: object_store::ObjectStore,
    stream: stream::Stream<()>,
}

impl AttachmentsBucket {
    async fn new(jetstream: &jetstream::Context, store: object_store::ObjectStore) -> Result<Self> {
        // This constructs a handle without a STREAM.INFO request.
        let stream = jetstream
            .get_stream_no_info(object_store_stream_name())
            .await
            .context("build the attachment object-store stream handle")?;
        Ok(Self { store, stream })
    }
}

impl Deref for AttachmentsBucket {
    type Target = object_store::ObjectStore;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

/// Put a media blob into the object store.
///
/// The blob is stored at the key specified by `CidUrl::kv_key()`.
pub async fn put_media(
    store: &AttachmentsBucket,
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
    store: &AttachmentsBucket,
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
    store: &AttachmentsBucket,
    blob: MediaBlob<'_>,
    chunk_size: Option<usize>,
) -> Result<()> {
    let MediaBlob {
        url,
        bytes,
        mime_type,
    } = blob;
    let key = url.kv_key();
    if leader_media_info(store, &key)
        .await?
        .is_some_and(|info| !info.deleted)
    {
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

/// Object size without downloading its body. Returns `None` when the object is
/// missing. Lets `stat`/`exists` probes stay cheap instead of consuming a
/// reader's cache or download budget.
pub async fn media_size(store: &object_store::ObjectStore, url: &CidUrl) -> Result<Option<u64>> {
    let key = url.kv_key();
    match store.info(&key).await {
        Ok(info) if info.deleted => Ok(None),
        Ok(info) => Ok(Some(info.size as u64)),
        Err(error) if error.kind() == object_store::InfoErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(anyhow::Error::from(error)).with_context(|| format!("stat media blob '{}'", key))
        }
    }
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
    let mime_type = media_content_type(object.info()).to_owned();
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
) -> Result<AttachmentsBucket> {
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
        return AttachmentsBucket::new(jetstream, store).await;
    }
    if let Err(error) = raise_object_store_replicas(jetstream, replicas).await {
        log::warn!(
            "could not reconcile replicas for attachment object store '{ATTACHMENTS_BUCKET}': {error:#}"
        );
    }
    open_attachments_bucket(jetstream).await
}

async fn open_attachments_bucket(jetstream: &jetstream::Context) -> Result<AttachmentsBucket> {
    let store = jetstream
        .get_object_store(ATTACHMENTS_BUCKET)
        .await
        .context("open NATS session attachment object store")?;
    AttachmentsBucket::new(jetstream, store).await
}

/// Get an existing attachments bucket, or None if it doesn't exist.
pub async fn optional_attachments_bucket(
    jetstream: &jetstream::Context,
) -> Result<Option<AttachmentsBucket>> {
    match jetstream.get_stream(object_store_stream_name()).await {
        Ok(_) => open_attachments_bucket(jetstream).await.map(Some),
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
        delete_media_object(&store, name)
            .await
            .with_context(|| format!("delete media object '{}'", name))?;
    }

    Ok(names.len())
}

fn media_metadata_subject(key: &str) -> String {
    format!(
        "$O.{ATTACHMENTS_BUCKET}.M.{}",
        base64::engine::general_purpose::URL_SAFE.encode(key)
    )
}

fn media_chunk_subject(nuid: &str) -> String {
    format!("$O.{ATTACHMENTS_BUCKET}.C.{nuid}")
}

/// Read metadata for an object key (`CidUrl::kv_key()`) from the stream leader.
/// Returns `None` only if the key is absent. Tombstones are returned with
/// `deleted` set so callers can distinguish them from live objects.
pub async fn leader_media_info(
    store: &AttachmentsBucket,
    key: &str,
) -> Result<Option<object_store::ObjectInfo>> {
    let subject = media_metadata_subject(key);
    let message = retry_transient(|| async {
        match store.stream.get_last_raw_message_by_subject(&subject).await {
            Ok(message) => Ok(Some(message)),
            Err(error) if error.kind() == stream::LastRawMessageErrorKind::NoMessageFound => {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    })
    .await
    .with_context(|| format!("read media metadata for '{key}'"))?;
    message
        .map(|message| serde_json::from_slice(&message.payload))
        .transpose()
        .with_context(|| format!("decode media metadata for '{key}'"))
}

/// Content type reported by `get_media` for this object's metadata.
pub fn media_content_type(info: &object_store::ObjectInfo) -> &str {
    info.metadata
        .get(CONTENT_TYPE_METADATA_KEY)
        .map(String::as_str)
        .unwrap_or("application/octet-stream")
}

/// Read the chunks for live metadata from [`leader_media_info`]. Uses the
/// stream leader for every chunk, without re-reading metadata or opening a
/// consumer. Returns an error on missing chunks or a size mismatch. The
/// caller must verify the content digest if needed.
pub async fn read_media_from_leader(
    store: &AttachmentsBucket,
    info: &object_store::ObjectInfo,
) -> Result<Vec<u8>> {
    ensure!(!info.deleted, "media object '{}' is deleted", info.name);
    // next_by_subj is a filter, so don't use metadata that could match other objects.
    ensure!(
        !info.nuid.is_empty()
            && info
                .nuid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid chunk ID for media object '{}'",
        info.name
    );
    let subject = media_chunk_subject(&info.nuid);
    let mut sequence = 0;
    let mut bytes = Vec::new();
    for index in 0..info.chunks {
        let chunk = retry_transient(|| async {
            Ok(store
                .stream
                .get_first_raw_message_by_subject(&subject, sequence)
                .await?)
        })
        .await
        .with_context(|| {
            format!(
                "read chunk {} of {} for '{}'",
                index + 1,
                info.chunks,
                info.name
            )
        })?;
        ensure!(
            chunk.payload.len() <= info.size - bytes.len(),
            "media object '{}' exceeds declared size of {} bytes",
            info.name,
            info.size
        );
        bytes.extend_from_slice(&chunk.payload);
        sequence = chunk.sequence + 1;
    }
    ensure!(
        bytes.len() == info.size,
        "media object '{}' has {} bytes, expected {}",
        info.name,
        bytes.len(),
        info.size
    );
    Ok(bytes)
}

/// Remove one object, leaving nothing behind. The object store's own `delete`
/// writes a metadata entry marked deleted, which then stays forever. The
/// chunks go first, so a failure between the two purges leaves an entry the
/// next deletion still finds, rather than chunks nothing points at.
async fn delete_media_object(store: &AttachmentsBucket, name: &str) -> Result<()> {
    let Some(info) = leader_media_info(store, name).await? else {
        return Ok(());
    };
    store
        .stream
        .purge()
        .filter(media_chunk_subject(&info.nuid))
        .await
        .context("purge media object chunks")?;
    store
        .stream
        .purge()
        .filter(media_metadata_subject(name))
        .await
        .context("purge media object metadata")?;
    Ok(())
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
