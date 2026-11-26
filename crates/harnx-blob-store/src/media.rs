//! Media object store operations on the `harnx_attachments` bucket.

use std::{collections::HashMap, future::Future, ops::Deref, time::Duration};

use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, message::StreamMessage, object_store, stream};
use base64::Engine as _;
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, time::Instant};

use harnx_core::cid_url::CidUrl;
use harnx_nats_common::leader_reads::{retry_transient, retry_transient_until};

/// JetStream object-store bucket containing durable session attachment blobs.
pub const ATTACHMENTS_BUCKET: &str = "harnx_attachments";

/// Content type metadata key for the object store.
const CONTENT_TYPE_METADATA_KEY: &str = "content_type";

// Leave room beyond the 15 s recovery budget for sequential reads of large blobs.
const LEADER_MEDIA_READ_TIMEOUT: Duration = Duration::from_secs(30);

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

/// The outcome of one media put: a dedup hit on an existing object, or a
/// new committed version with the NUID that identifies exactly this
/// attempt's chunks and metadata version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPut {
    /// An object with this key already existed and was left untouched; the
    /// caller owns nothing and must not clean anything up.
    DedupHit,
    /// A new object version was committed under this NUID.
    Written { nuid: String },
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
    put_media_versioned(
        store,
        MediaBlob {
            url,
            bytes,
            mime_type,
        },
    )
    .await
    .map(|_| ())
}

/// [`put_media`] with the write outcome: a dedup hit or the NUID of the new
/// version. The NUID scopes a later cleanup to exactly what this put
/// committed (see [`purge_media_version`]).
pub async fn put_media_versioned(
    store: &AttachmentsBucket,
    blob: MediaBlob<'_>,
) -> Result<MediaPut> {
    put_media_chunked(store, blob, None).await
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
    put_media_chunked(store, blob, Some(max_chunk_bytes.min(128 * 1024)))
        .await
        .map(|_| ())
}

/// Identifies the metadata message one put wrote. `ObjectStore::put` does
/// not return the NUID it generated when the put fails, so the
/// `PurgeOldChunks` recovery (which fires after the new metadata is
/// acknowledged) can only prove the version it reads back is this put's own
/// by a marker stored in the metadata itself. Readers that only want the
/// content type ignore it.
const PUT_ID_METADATA_KEY: &str = "harnx_put_id";

/// A fresh put identifier: content-addressed keys can be written
/// concurrently by this process or another replica, so a random UUID (not a
/// per-process counter or the wall clock) keeps the recovery's version
/// proof unique across replicas.
fn fresh_put_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn put_media_chunked(
    store: &AttachmentsBucket,
    blob: MediaBlob<'_>,
    chunk_size: Option<usize>,
) -> Result<MediaPut> {
    let MediaBlob {
        url,
        bytes,
        mime_type,
    } = blob;
    let key = url.kv_key();
    // The dedup probe is optional, but its failure is the typed evidence a
    // later put failure has none of: `ObjectStore::put` stringifies its
    // chunk and metadata publish errors, so a hung JetStream behind a live
    // TCP connection looks like a store failure, not unavailability. Keep
    // the probe's error so the put's failure can chain it.
    let mut probe_error = None;
    match leader_media_info(store, &key).await {
        Ok(Some(info)) if !info.deleted => return Ok(MediaPut::DedupHit),
        Ok(_) => {}
        Err(error) => {
            // Dedup is optional: a content-addressed put can be repeated.
            log::debug!("media dedup probe failed; attempting put: {error:#}");
            probe_error = Some(error);
        }
    }
    let mut metadata = HashMap::new();
    metadata.insert(CONTENT_TYPE_METADATA_KEY.to_string(), mime_type.to_string());
    let put_id = fresh_put_id();
    metadata.insert(PUT_ID_METADATA_KEY.to_string(), put_id.clone());
    let object = object_store::ObjectMetadata {
        name: key.clone(),
        description: Some(format!("Attachment {}", url)),
        metadata,
        chunk_size,
        ..Default::default()
    };
    let mut cursor = std::io::Cursor::new(bytes);
    match store.put(object, &mut cursor).await {
        Ok(written) => Ok(MediaPut::Written { nuid: written.nuid }),
        Err(error) if error.kind() == object_store::PutErrorKind::PurgeOldChunks => {
            committed_after_failed_cleanup(store, &key, &put_id, error).await
        }
        Err(error) => Err(put_failure(probe_error, error, &key)),
    }
}

/// The put's failure, with the dedup probe's typed evidence chained when the
/// probe also failed. `ObjectStore::put` publishes chunks and metadata with
/// stringified causes, so the probe's typed chain — which names the same
/// JetStream the put then failed against — is the availability evidence a
/// caller's classification needs. The put's own failure stays visible as
/// the context message.
fn put_failure(
    probe_error: Option<anyhow::Error>,
    put_error: object_store::PutError,
    key: &str,
) -> anyhow::Error {
    let put_error =
        anyhow::Error::new(put_error).context(format!("failed to put media blob '{key}'"));
    match probe_error {
        Some(probe) => probe.context(format!(
            "failed to put media blob '{key}' after a failed dedup probe: {put_error:#}"
        )),
        None => put_error,
    }
}

/// `ObjectStore::put` acknowledges the new metadata before purging the
/// previous version's chunks, so a `PurgeOldChunks` failure means this put's
/// version is committed. Report the commit, not the cleanup: the caller's
/// recheck and its own version-scoped cleanup still apply to the committed
/// NUID, and the orphaned old chunks are the known invisible-orphan class.
///
/// The NUID is read back from the leader because the failed put does not
/// return it — and only reported when that version is provably this put's
/// own, by the `PUT_ID_METADATA_KEY` marker the put stored in its metadata.
/// The leader's current version may instead be a concurrent same-key put's
/// (it superseded this one between the commit and the re-read); reporting
/// its NUID would hand the caller's version-scoped cleanup someone else's
/// object, so anything unproven — a foreign or deleted version, no record,
/// or a failed re-read — surfaces as an error and purges nothing. Both
/// failures stay visible when the confirmation read fails too.
async fn committed_after_failed_cleanup(
    store: &AttachmentsBucket,
    key: &str,
    put_id: &str,
    cleanup_error: object_store::PutError,
) -> Result<MediaPut> {
    log::warn!(
        "committed media blob '{key}' but failed to purge the previous version's chunks: {cleanup_error}"
    );
    match leader_media_version(store, key).await {
        Ok(Some((_, info))) => {
            if !info.deleted && info.metadata.get(PUT_ID_METADATA_KEY).is_some_and(|id| id == put_id) {
                return Ok(MediaPut::Written {
                    nuid: info.nuid.clone(),
                });
            }
            Err(anyhow::Error::msg(format!(
                "committed media blob '{key}' but cannot confirm its version: the leader's current metadata is not this put's own (deleted: {}, this put's id present: {}): {cleanup_error}",
                info.deleted,
                info.metadata.contains_key(PUT_ID_METADATA_KEY)
            )))
        }
        Ok(None) => Err(anyhow::Error::msg(format!(
            "committed media blob '{key}' but cannot confirm its version: no leader metadata after a failed chunk cleanup: {cleanup_error}"
        ))),
        Err(read_error) => Err(read_error.context(format!(
            "committed media blob '{key}' but cannot confirm its version after a failed chunk cleanup: {cleanup_error}"
        ))),
    }
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

/// The JetStream stream that backs the attachments bucket, for callers that
/// probe its existence directly.
pub fn attachments_stream_name() -> String {
    object_store_stream_name()
}

fn stream_missing(kind: &jetstream::context::GetStreamErrorKind) -> bool {
    matches!(
        kind,
        jetstream::context::GetStreamErrorKind::JetStream(error)
            if error.kind() == jetstream::ErrorCode::STREAM_NOT_FOUND
    )
}

/// Whether a failed stream lookup definitively means the attachment
/// object-store stream is gone, as opposed to the lookup request itself
/// failing (a timeout is not an absence).
pub fn attachments_stream_missing(kind: &jetstream::context::GetStreamErrorKind) -> bool {
    stream_missing(kind)
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

/// Live canonical attachment objects for one owner (`media/<owner>/…`), for
/// read-only storage stats. Objects under other owners, pre-#2157 legacy names
/// (`<sha256(session_id)>/<sha256(cid)>`, which no current owner key matches)
/// and deleted objects are excluded. `logical_bytes` is the sum of object
/// metadata sizes, not the bucket's physical backing-stream bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachmentSummary {
    pub count: usize,
    pub logical_bytes: u64,
}

/// Count one session owner's live canonical media objects. The prefix is exact
/// — `media/<owner>/` with the separator — so a neighbouring owner's objects
/// are never counted, and no object payload is read.
pub async fn session_attachment_summary(
    store: &AttachmentsBucket,
    owner: &str,
) -> Result<SessionAttachmentSummary> {
    use futures_util::StreamExt;
    let prefix = format!("media/{owner}/");
    let mut summary = SessionAttachmentSummary::default();
    let mut objects = store.list().await.context("list media objects")?;
    while let Some(info) = objects.next().await {
        let info = info.context("read media object metadata")?;
        if info.deleted || !info.name.starts_with(&prefix) {
            continue;
        }
        summary.count += 1;
        summary.logical_bytes += info.size as u64;
    }
    Ok(summary)
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
    Ok(leader_media_version(store, key)
        .await?
        .map(|(_, info)| info))
}

/// The current metadata version of one media object as the stream leader
/// has it: the stream sequence of its metadata message plus the decoded
/// version. `None` when the object has no metadata message.
pub(crate) async fn leader_media_version(
    store: &AttachmentsBucket,
    key: &str,
) -> Result<Option<(u64, object_store::ObjectInfo)>> {
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
        .map(|message| {
            let info: object_store::ObjectInfo = serde_json::from_slice(&message.payload)
                .with_context(|| format!("decode media metadata for '{key}'"))?;
            Ok((message.sequence, info))
        })
        .transpose()
}

/// Best-effort removal of exactly one committed object version: the
/// object's metadata message, but only while it still names the same NUID
/// — a concurrent put under the same content-addressed key owns any newer
/// version and must not be removed — then the chunks of that one put
/// attempt (identified by the NUID [`put_media_versioned`] returned).
/// Never call this for a dedup hit: the caller does not own the object.
///
/// The metadata message is deleted first, on purpose: if the deletion
/// fails transiently, a partial cleanup leaves at most orphan chunks
/// (invisible; sweeping them is P2), never live metadata pointing at
/// purged chunks — a later same-CID put would dedup-hit that chunkless
/// object and hand out refs that cannot be read.
///
/// A missing metadata message still purges this NUID's chunks: no version
/// of ours is left named by the object.
pub async fn purge_media_version(
    store: &AttachmentsBucket,
    url: &CidUrl,
    nuid: &str,
) -> Result<()> {
    let key = url.kv_key();
    if let Some((sequence, info)) = leader_media_version(store, &key).await? {
        if info.nuid == nuid {
            store
                .stream
                .delete_message(sequence)
                .await
                .with_context(|| format!("delete metadata of media blob '{key}' (nuid {nuid})"))?;
        }
    }
    let chunk_subject = format!("$O.{}.C.{}", ATTACHMENTS_BUCKET, nuid);
    store
        .stream
        .purge()
        .filter(&chunk_subject)
        .await
        .with_context(|| format!("purge chunks of media blob '{key}' (nuid {nuid})"))?;
    Ok(())
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
/// consumer. All chunks and their retries share one 30-second deadline.
/// Returns an error on timeout, missing chunks, or a size mismatch. The caller
/// must verify the content digest if needed.
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
    read_media_chunks(info, |sequence| {
        store
            .stream
            .get_first_raw_message_by_subject(&subject, sequence)
            .err_into()
    })
    .await
}

async fn read_media_chunks<Fut>(
    info: &object_store::ObjectInfo,
    mut get_chunk: impl FnMut(u64) -> Fut,
) -> Result<Vec<u8>>
where
    Fut: Future<Output = Result<StreamMessage>>,
{
    let deadline = Instant::now() + LEADER_MEDIA_READ_TIMEOUT;
    let mut sequence = 0;
    let mut bytes = Vec::new();
    for index in 0..info.chunks {
        let chunk = retry_transient_until(deadline, || get_chunk(sequence))
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
    // A ready response can win the race with the timeout in the final poll.
    // Return the same typed marker `retry_until` returns when the deadline
    // expires with the last chunk still pending, so a caller matching the
    // marker sees one failure shape for one expiry.
    if Instant::now() >= deadline {
        return Err(anyhow::Error::new(
            harnx_nats_common::recovery::RecoveryDeadlineExceeded,
        ))
        .with_context(|| format!("read media chunks of '{}'", info.name));
    }
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
    use std::time::Duration;

    use async_nats::jetstream::{message::StreamMessage, stream};
    use tokio::time::{sleep, Instant};

    use super::{read_media_chunks, ATTACHMENTS_BUCKET};

    #[tokio::test(start_paused = true)]
    async fn leader_chunk_reads_share_one_deadline() {
        let info = serde_json::from_value(serde_json::json!({
            "name": "attachment", "bucket": ATTACHMENTS_BUCKET, "nuid": "chunks",
            "chunks": 3, "size": 3,
        }))
        .unwrap();
        let start = Instant::now();
        let mut requests = Vec::new();
        let result = read_media_chunks(&info, |sequence| {
            let first_attempt = requests.is_empty();
            requests.push(sequence);
            async move {
                if first_attempt {
                    return Err(
                        stream::RawMessageError::new(stream::RawMessageErrorKind::Other).into(),
                    );
                }
                // Each chunk would finish within its own 15 s budget, but the
                // third cannot finish before 30 s from the start of the read.
                sleep(Duration::from_secs(12)).await;
                Ok(StreamMessage {
                    subject: "attachment-chunks".into(),
                    sequence,
                    headers: Default::default(),
                    payload: "x".into(),
                    time: async_nats::datetime::now(),
                })
            }
        })
        .await;

        let error = result.expect_err("the aggregate deadline must discard partial bytes");
        // The retry window's expiry surfaces as the typed recovery marker
        // (`RecoveryDeadlineExceeded`), so availability classification can
        // match it without error text.
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<harnx_nats_common::recovery::RecoveryDeadlineExceeded>()),
            "the deadline must surface as the typed recovery marker: {error:#}"
        );
        assert!(error.to_string().contains("read chunk 3 of 3"), "{error:#}");
        assert_eq!(
            requests,
            [0, 0, 1, 2],
            "retry the first chunk at the same sequence"
        );
        assert!(start.elapsed() < Duration::from_secs(31));
    }

    #[test]
    fn put_failure_chains_the_dedup_probe_evidence() {
        use async_nats::jetstream::object_store::{PutError, PutErrorKind};
        use harnx_nats_common::recovery::RecoveryDeadlineExceeded;

        // The probe failed with the typed recovery marker; the put then
        // failed with a stringified publish error. The chain must keep the
        // probe's typed evidence for availability classification, and the
        // put's own failure must stay visible.
        let probe = anyhow::Error::new(RecoveryDeadlineExceeded)
            .context("read media metadata for 'media/o/k'");
        let error = super::put_failure(
            Some(probe),
            PutError::new(PutErrorKind::PublishChunks),
            "media/o/k",
        );
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<RecoveryDeadlineExceeded>()),
            "the probe's typed marker must survive: {error:#}"
        );
        assert!(
            error.to_string().contains("failed dedup probe"),
            "the put failure stays visible: {error:#}"
        );

        // Without a failed probe the put failure stands on its own: no
        // fabricated availability evidence.
        let error = super::put_failure(None, PutError::new(PutErrorKind::PublishMetadata), "k");
        assert!(
            !error
                .chain()
                .any(|cause| cause.is::<RecoveryDeadlineExceeded>()),
            "a put failure alone must not claim the recovery marker: {error:#}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn leader_chunk_read_past_deadline_surfaces_the_typed_marker() {
        let info = serde_json::from_value(serde_json::json!({
            "name": "attachment", "bucket": ATTACHMENTS_BUCKET, "nuid": "chunks",
            "chunks": 3, "size": 3,
        }))
        .unwrap();
        // Every chunk answers, but the aggregate finishes at the deadline:
        // the expiry must surface as the typed recovery marker either way —
        // from `retry_until` if the last chunk is still pending, or from
        // the final clock check if it just made it.
        let result = read_media_chunks(&info, |sequence| async move {
            sleep(Duration::from_secs(10)).await;
            Ok(StreamMessage {
                subject: "attachment-chunks".into(),
                sequence,
                headers: Default::default(),
                payload: "x".into(),
                time: async_nats::datetime::now(),
            })
        })
        .await;

        let error = result.expect_err("the aggregate deadline must reject the read");
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<harnx_nats_common::recovery::RecoveryDeadlineExceeded>()),
            "the expiry must surface as the typed recovery marker: {error:#}"
        );
    }

    /// The synthesized `PurgeOldChunks` failure the recovery tests pass to
    /// `committed_after_failed_cleanup`: the state it implies was built by
    /// [`committed_cleanup_state`].
    fn failed_chunk_cleanup() -> async_nats::jetstream::object_store::PutError {
        async_nats::jetstream::object_store::PutError::new(
            async_nats::jetstream::object_store::PutErrorKind::PurgeOldChunks,
        )
    }

    /// The committed state `ObjectStore::put` leaves behind when the new
    /// metadata is acknowledged but the old-chunk purge fails: a first
    /// version, then a raw second put on the same key carrying the
    /// `harnx_put_id` marker ("committed-put-id") a real put's own version
    /// would have. A purge-denying stream cannot build this end to end
    /// (rollup publishes need the same permission), so the second put is
    /// raw and the cleanup failure is synthesized by the callers. Returns
    /// `None` the same way `spawn_nats_server` does when the broker binary
    /// is unavailable.
    async fn committed_cleanup_state(
        label: &str,
    ) -> anyhow::Result<
        Option<(
            harnx_test_bins::NatsServerHandle,
            super::AttachmentsBucket,
            String,
        )>,
    > {
        harnx_core::require_nextest();
        let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(None);
        };
        let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let store = super::ensure_attachments_bucket(&jetstream, 1).await?;
        let session = harnx_core::cid_url::SessionRef::new(None, "cleanup".into())?;
        let url = crate::media_cid_url(&session, label);
        let key = url.kv_key();
        super::put_media(&store, &url, b"first", "text/plain").await?;
        let object = async_nats::jetstream::object_store::ObjectMetadata {
            name: key.clone(),
            description: Some("Attachment".into()),
            metadata: [
                (
                    super::CONTENT_TYPE_METADATA_KEY.to_string(),
                    "text/plain".to_string(),
                ),
                (
                    super::PUT_ID_METADATA_KEY.to_string(),
                    "committed-put-id".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let mut cursor = std::io::Cursor::new(b"second");
        store.put(object, &mut cursor).await?;
        Ok(Some((server, store, key)))
    }

    /// The `PurgeOldChunks` recovery reports the committed version, and only
    /// when the re-read metadata is provably this put's own by its
    /// `harnx_put_id` marker: the reported NUID names the committed version
    /// and it reads back whole.
    #[tokio::test]
    async fn purge_failure_after_commit_reports_the_committed_version() -> anyhow::Result<()> {
        use anyhow::Context as _;
        let Some((_server, store, key)) = committed_cleanup_state("purgefail").await? else {
            return Ok(());
        };

        let outcome = super::committed_after_failed_cleanup(
            &store,
            &key,
            "committed-put-id",
            failed_chunk_cleanup(),
        )
        .await?;
        let super::MediaPut::Written { nuid } = &outcome else {
            panic!("the recovery reports the committed version, not the stale one: {outcome:?}");
        };
        let info = super::leader_media_info(&store, &key)
            .await?
            .context("committed version")?;
        assert_eq!(
            &info.nuid, nuid,
            "the reported NUID names the committed version"
        );
        assert_eq!(
            super::read_media_from_leader(&store, &info).await?,
            b"second",
            "the committed version reads back whole"
        );
        Ok(())
    }

    /// The `PurgeOldChunks` recovery reports nothing it cannot prove is this
    /// put's own version: another put's marker, a tombstone, and an absent
    /// record all error — with the cleanup failure visible — and the foreign
    /// version is left in place.
    #[tokio::test]
    async fn purge_failure_unproven_version_errors_and_purges_nothing() -> anyhow::Result<()> {
        use anyhow::Context as _;
        let Some((_server, store, key)) = committed_cleanup_state("purgefail-unproven").await?
        else {
            return Ok(());
        };

        // Another put's marker: not a commit of this put's own.
        let error = super::committed_after_failed_cleanup(
            &store,
            &key,
            "another-put-id",
            failed_chunk_cleanup(),
        )
        .await
        .expect_err("a foreign version must not be reported as committed");
        assert!(
            error.to_string().contains("not this put's own"),
            "the confirmation failure surfaces: {error:#}"
        );
        assert!(
            error.to_string().contains("failed purging old chunks"),
            "the cleanup failure stays visible: {error:#}"
        );
        let info = super::leader_media_info(&store, &key)
            .await?
            .context("foreign version stays")?;
        assert_eq!(
            super::read_media_from_leader(&store, &info).await?,
            b"second",
            "the foreign version still reads back whole: nothing was purged"
        );

        // A tombstone is not a commit either: the same put id on a deleted
        // version errors instead of reporting it.
        store.delete(&key).await?;
        let error = super::committed_after_failed_cleanup(
            &store,
            &key,
            "committed-put-id",
            failed_chunk_cleanup(),
        )
        .await
        .expect_err("a deleted version must not be reported as committed");
        assert!(
            error.to_string().contains("not this put's own"),
            "the deleted version is rejected the same way: {error:#}"
        );

        // Nothing committed under a fresh key surfaces too, not a phantom
        // commit.
        let fresh = crate::media_cid_url(
            &harnx_core::cid_url::SessionRef::new(None, "cleanup".into())?,
            "purgefail-absent",
        )
        .kv_key();
        let error = super::committed_after_failed_cleanup(
            &store,
            &fresh,
            "committed-put-id",
            failed_chunk_cleanup(),
        )
        .await
        .expect_err("nothing was committed");
        assert!(
            error.to_string().contains("no leader metadata"),
            "the absent-record case surfaces: {error:#}"
        );
        Ok(())
    }

    /// The recovery's confirmation read can fail on its own; both failures
    /// must surface, and nothing is reported as committed.
    #[tokio::test]
    async fn purge_failure_confirmation_read_failure_surfaces_both_failures() -> anyhow::Result<()>
    {
        let Some((mut server, store, key)) = committed_cleanup_state("purgefail-read").await?
        else {
            return Ok(());
        };

        // The confirmation read cannot reach the broker: both failures
        // must surface, and the recovery must not report a commit.
        server.kill()?;
        server.wait()?;
        let error = super::committed_after_failed_cleanup(
            &store,
            &key,
            "committed-put-id",
            failed_chunk_cleanup(),
        )
        .await
        .expect_err("an unconfirmable recovery must not report a commit");
        assert!(
            error.to_string().contains("cannot confirm its version"),
            "the confirmation failure surfaces: {error:#}"
        );
        assert!(
            error.to_string().contains("failed purging old chunks"),
            "the cleanup failure stays visible: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("read media metadata"),
            "the read failure keeps its own context: {error:#}"
        );
        Ok(())
    }

    /// The production put path really stamps the put-id marker the
    /// recovery's version proof reads: `put_media` (through
    /// `put_media_chunked`, the only production metadata writer) leaves
    /// `PUT_ID_METADATA_KEY` on the stored metadata, as a fresh UUID.
    #[tokio::test]
    async fn put_media_stamps_put_id_marker_on_stored_metadata() -> anyhow::Result<()> {
        use anyhow::Context as _;
        harnx_core::require_nextest();
        let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let store = super::ensure_attachments_bucket(&jetstream, 1).await?;
        let session = harnx_core::cid_url::SessionRef::new(None, "putidmark".into())?;
        let url = crate::media_cid_url(&session, "stamp-put-id");
        let key = url.kv_key();
        super::put_media(&store, &url, b"some attachment bytes", "text/plain").await?;
        let info = super::leader_media_info(&store, &key)
            .await?
            .context("stored metadata")?;
        let marker = info
            .metadata
            .get(super::PUT_ID_METADATA_KEY)
            .context("the put stamps its put-id marker on the stored metadata")?;
        assert!(
            uuid::Uuid::parse_str(marker).is_ok(),
            "the marker is a fresh UUID, not a derived or shared value: {marker}"
        );
        Ok(())
    }

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

    /// `session_attachment_summary` attributes objects by the exact
    /// `media/<owner>/` prefix: canonical objects count with their logical
    /// bytes, foreign owners, neighbour-prefixed names, pre-#2157 unscoped
    /// legacy names and deleted objects never do.
    #[tokio::test]
    async fn nats_session_attachment_summary_attributes_exact_owner_prefix() -> anyhow::Result<()> {
        use anyhow::Context as _;
        harnx_core::require_nextest();
        let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let store = super::ensure_attachments_bucket(&jetstream, 1).await?;
        let session = harnx_core::cid_url::SessionRef::new(None, "ownership-session".into())?;
        let owner = session.owner();
        let hash_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let hash_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let hash_c = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        let hash_d = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        // Two canonical objects for the owner: 12 + 1 logical bytes.
        super::put_media(
            &store,
            &crate::media_cid_url(&session, hash_a),
            b"twelve bytes",
            "text/plain",
        )
        .await?;
        super::put_media(
            &store,
            &crate::media_cid_url(&session, hash_b),
            b"x",
            "text/plain",
        )
        .await?;
        // A foreign owner's object.
        let other = harnx_core::cid_url::SessionRef::new(None, "other-session".into())?;
        super::put_media(
            &store,
            &crate::media_cid_url(&other, hash_c),
            b"zzz",
            "text/plain",
        )
        .await?;
        // Names the exact prefix must reject: no separator after the owner, a
        // longer name the owner is a prefix of, and a pre-#2157 unscoped
        // legacy name.
        async fn raw_object(
            store: &super::AttachmentsBucket,
            name: &str,
            bytes: &'static [u8],
        ) -> anyhow::Result<()> {
            let mut data = bytes;
            store
                .put(
                    async_nats::jetstream::object_store::ObjectMetadata {
                        name: name.to_string(),
                        ..Default::default()
                    },
                    &mut data,
                )
                .await
                .with_context(|| format!("put raw object '{name}'"))?;
            Ok(())
        }
        raw_object(&store, &format!("media/{owner}"), b"no-slash").await?;
        raw_object(&store, &format!("media/{owner}NEIGH/h"), b"neighbour").await?;
        raw_object(&store, &format!("legacy/{hash_d}"), b"legacy").await?;
        // A deleted object under the owner's prefix: a tombstone, not a live
        // object, so it must not count.
        super::put_media(
            &store,
            &crate::media_cid_url(&session, hash_d),
            b"gone",
            "text/plain",
        )
        .await?;
        store.delete(format!("media/{owner}/{hash_d}")).await?;

        let summary = super::session_attachment_summary(&store, &owner).await?;
        assert_eq!(
            (summary.count, summary.logical_bytes),
            (2, 13),
            "only the two live canonical objects count, with logical bytes"
        );
        let other_summary = super::session_attachment_summary(&store, &other.owner()).await?;
        assert_eq!((other_summary.count, other_summary.logical_bytes), (1, 3));
        Ok(())
    }
}
