//! Durable attachment blobs for NATS-backed sessions.
//!
//! Session logs keep only content-addressed `cid:` references. The matching
//! bytes live in one JetStream object store under a session-scoped object
//! name, so payloads stay below NATS message limits and session deletion can
//! garbage-collect exactly the blobs owned by that session.

use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, object_store};
use harnx_blob_store::media::{
    ensure_attachments_bucket, get_media, optional_attachments_bucket, put_media,
};
use harnx_core::attachments::{
    cid_for_data_url_with_session, collect_cid_refs, read_attachment_async,
    store_attachment_bytes_async, CID_PREFIX,
};
use harnx_core::cid_url::{CidUrl, SessionRef};
use harnx_core::message::{MessageContent, MessageContentPart};
use std::path::Path;

/// JetStream object-store bucket containing durable session attachment blobs.
pub use harnx_blob_store::media::ATTACHMENTS_BUCKET as SESSION_ATTACHMENTS_BUCKET;

/// Context for attachment operations tied to a specific session.
#[derive(Clone, Copy)]
pub struct AttachmentLocation<'a> {
    jetstream: &'a jetstream::Context,
    replicas: usize,
    /// The session identity for producing cid: URLs.
    session: &'a SessionRef,
}

impl<'a> AttachmentLocation<'a> {
    /// Describe the object-store location for one session's attachments.
    ///
    /// Parameters:
    /// - `jetstream`: The JetStream context
    /// - `replicas`: Number of replicas for the object store
    /// - `session`: The session identity for cid: URL production
    pub fn new(
        jetstream: &'a jetstream::Context,
        replicas: usize,
        session: &'a SessionRef,
    ) -> Self {
        Self {
            jetstream,
            replicas,
            session,
        }
    }
}

/// Worker-side attachment lifecycle for one NATS session activation.
pub(crate) struct SessionAttachmentSync {
    jetstream: jetstream::Context,
    config: crate::config::GlobalConfig,
    replicas: usize,
    session: SessionRef,
}

impl SessionAttachmentSync {
    pub(crate) fn replicas(&self) -> usize {
        self.replicas
    }

    pub(crate) async fn prepare(
        jetstream: jetstream::Context,
        config: crate::config::GlobalConfig,
        cluster_key: &str,
        session: SessionRef,
    ) -> Result<Self> {
        let config_snapshot = config.read().clone();
        let replicas = config_snapshot
            .resolve_nats_server(cluster_key)
            .await?
            .resolved_replicas();
        hydrate_session_attachments(&jetstream, &config, replicas, session.clone()).await?;
        Ok(Self {
            jetstream,
            config,
            replicas,
            session,
        })
    }

    pub(crate) async fn finish<T>(self, result: Result<T>) -> Result<T> {
        let attachment_sync =
            sync_session_attachments(&self.jetstream, &self.config, self.replicas).await;
        match result {
            Ok(value) => {
                attachment_sync?;
                Ok(value)
            }
            Err(error) => {
                if let Err(sync_error) = attachment_sync {
                    log::warn!(
                        "failed to sync session attachments after turn error: session={} error={sync_error:#}",
                        self.session.session_id
                    );
                }
                Err(error)
            }
        }
    }
}

fn parse_media_url(cid: &str) -> Result<CidUrl> {
    let cid_url = CidUrl::parse(cid).with_context(|| format!("parse attachment URL '{cid}'"))?;
    match cid_url {
        url @ CidUrl::Media { .. } => Ok(url),
        CidUrl::Plan { .. } => bail!("expected media URL for attachment"),
    }
}

fn local_cid(cid_url: &CidUrl) -> Result<String> {
    match cid_url {
        CidUrl::Media { hash, .. } => Ok(format!("{CID_PREFIX}{hash}")),
        CidUrl::Plan { .. } => bail!("expected media URL for attachment"),
    }
}

/// Read one attachment from the session-scoped JetStream object store.
///
/// A missing bucket or object returns `Ok(None)`. The bucket is created only
/// when another attachment operation has already established it.
pub async fn get_session_attachment(
    jetstream: &jetstream::Context,
    replicas: usize,
    cid: &str,
) -> Result<Option<(Vec<u8>, String)>> {
    if optional_attachments_bucket(jetstream).await?.is_none() {
        return Ok(None);
    }
    let store = ensure_attachments_bucket(jetstream, replicas).await?;
    let cid_url = parse_media_url(cid)?;
    get_media(&store, &cid_url).await
}

fn parse_data_url(data_url: &str) -> Result<(String, Vec<u8>)> {
    let rest = data_url
        .strip_prefix("data:")
        .context("attachment must be a data: URI")?;
    let (mime_type, encoded) = rest
        .split_once(";base64,")
        .context("attachment data URI must contain ;base64,")?;
    let bytes = harnx_core::crypto::base64_decode(encoded)
        .context("decode attachment data URI as base64")?;
    Ok((mime_type.to_string(), bytes))
}

struct ExternalizedAttachment {
    cid: String,
    mime_type: String,
    size: usize,
}

impl ExternalizedAttachment {
    fn marker(&self) -> String {
        crate::config::attachment_marker(&self.cid, &self.mime_type, self.size)
    }
}

async fn externalize_data_url(
    store: &object_store::ObjectStore,
    session: &SessionRef,
    data_url: &str,
) -> Result<ExternalizedAttachment> {
    let (mime_type, bytes) = parse_data_url(data_url)?;
    let cid = cid_for_data_url_with_session(session, data_url);
    let cid_url = parse_media_url(&cid).context("parse generated attachment URL")?;
    put_media(store, &cid_url, &bytes, &mime_type).await?;
    Ok(ExternalizedAttachment {
        cid: cid_url.to_string(),
        mime_type,
        size: bytes.len(),
    })
}

async fn externalize_cid_url(
    store: &object_store::ObjectStore,
    source_dir: Option<&Path>,
    cid: &str,
) -> Result<ExternalizedAttachment> {
    let cid_url = parse_media_url(cid)?;
    let stored = get_media(store, &cid_url).await?;
    let (bytes, mime_type) = match stored {
        Some(stored) => stored,
        None => read_and_upload_local_attachment(store, source_dir, cid, &cid_url).await?,
    };
    Ok(ExternalizedAttachment {
        cid: cid.to_string(),
        mime_type,
        size: bytes.len(),
    })
}

async fn read_and_upload_local_attachment(
    store: &object_store::ObjectStore,
    source_dir: Option<&Path>,
    cid: &str,
    cid_url: &CidUrl,
) -> Result<(Vec<u8>, String)> {
    let source_dir =
        source_dir.with_context(|| format!("attachment {cid} has no local source directory"))?;
    let (bytes, mime_type) = read_attachment_async(source_dir, &local_cid(cid_url)?).await?;
    put_media(store, cid_url, &bytes, &mime_type).await?;
    Ok((bytes, mime_type))
}

async fn externalize_part(
    store: &object_store::ObjectStore,
    session: &SessionRef,
    source_dir: Option<&Path>,
    part: &mut MessageContentPart,
) -> Result<Option<String>> {
    let MessageContentPart::ImageUrl { image_url } = part else {
        return Ok(None);
    };
    let attachment = if image_url.url.starts_with("data:") {
        let attachment = externalize_data_url(store, session, &image_url.url).await?;
        image_url.url.clone_from(&attachment.cid);
        Some(attachment)
    } else if image_url.url.starts_with(CID_PREFIX) {
        Some(externalize_cid_url(store, source_dir, &image_url.url).await?)
    } else {
        None
    };
    Ok(attachment.map(|attachment| attachment.marker()))
}

fn needs_externalization(part: &MessageContentPart) -> bool {
    let MessageContentPart::ImageUrl { image_url } = part else {
        return false;
    };
    image_url.url.starts_with("data:") || image_url.url.starts_with(CID_PREFIX)
}

/// Upload inline or locally-referenced attachments and rewrite inline data
/// URIs to durable `cid:` references suitable for the session log.
///
/// Inserts `[attachment: ...]` markers after each externalized image, deduplicating
/// if an identical marker already exists at the adjacent position.
pub async fn externalize_message_attachments(
    location: AttachmentLocation<'_>,
    content: &mut MessageContent,
    source_dir: Option<&Path>,
) -> Result<()> {
    let MessageContent::Array(parts) = content else {
        return Ok(());
    };
    if !parts.iter().any(needs_externalization) {
        return Ok(());
    }
    let store = ensure_attachments_bucket(location.jetstream, location.replicas).await?;
    let mut markers = Vec::new();
    for (index, part) in parts.iter_mut().enumerate() {
        if let Some(marker) = externalize_part(&store, location.session, source_dir, part).await? {
            markers.push((index, marker));
        }
    }
    insert_attachment_markers(parts, markers);
    Ok(())
}

/// Insert attachment markers at `index + 1` for each externalized image.
/// Deduplicates by checking if an identical marker already exists at the target position.
fn insert_attachment_markers(parts: &mut Vec<MessageContentPart>, markers: Vec<(usize, String)>) {
    for (index, marker) in markers.into_iter().rev() {
        let marker_exists = matches!(
            parts.get(index + 1),
            Some(MessageContentPart::Text { text }) if text == &marker
        );
        if !marker_exists {
            parts.insert(index + 1, MessageContentPart::Text { text: marker });
        }
    }
}

/// Download every referenced session blob that is missing from this worker's
/// local content-addressed cache.
pub async fn hydrate_attachment_refs(
    location: AttachmentLocation<'_>,
    dir: &Path,
    refs: &[String],
) -> Result<()> {
    if refs.is_empty() {
        return Ok(());
    }
    let store = match optional_attachments_bucket(location.jetstream).await? {
        Some(store) => store,
        None => ensure_attachments_bucket(location.jetstream, location.replicas).await?,
    };
    let hydration = AttachmentHydration { store: &store, dir };
    for cid in refs {
        hydrate_attachment_ref(&hydration, cid).await?;
    }
    Ok(())
}

struct AttachmentHydration<'a> {
    store: &'a object_store::ObjectStore,
    dir: &'a Path,
}

async fn hydrate_attachment_ref(hydration: &AttachmentHydration<'_>, cid: &str) -> Result<()> {
    let cid_url = parse_media_url(cid)?;
    let local_cid = local_cid(&cid_url)?;
    if let Ok((bytes, mime_type)) = read_attachment_async(hydration.dir, &local_cid).await {
        return put_media(hydration.store, &cid_url, &bytes, &mime_type).await;
    }

    let (bytes, mime_type) = get_media(hydration.store, &cid_url)
        .await?
        .with_context(|| format!("download attachment '{cid}'"))?;
    let stored_cid = store_attachment_bytes_async(hydration.dir, &bytes, &mime_type).await?;
    if stored_cid != local_cid {
        bail!("attachment digest mismatch: expected {local_cid}, got {stored_cid}");
    }
    Ok(())
}

/// Upload locally-created session attachments, such as image content returned
/// by tools, before the worker publishes the durable turn boundary.
pub async fn sync_session_attachments(
    jetstream: &jetstream::Context,
    config: &crate::config::GlobalConfig,
    replicas: usize,
) -> Result<()> {
    let (dir, refs) = {
        let config = config.read();
        let Some(sess) = config.session.as_ref() else {
            return Ok(());
        };
        let Some(dir) = crate::config::session_externalize::attachments_dir(sess) else {
            return Ok(());
        };
        (dir, collect_cid_refs(&sess.messages))
    };
    if refs.is_empty() {
        return Ok(());
    }
    let store = ensure_attachments_bucket(jetstream, replicas).await?;
    for cid in refs {
        let cid_url = parse_media_url(&cid)?;
        let (bytes, mime_type) = read_attachment_async(&dir, &local_cid(&cid_url)?).await?;
        put_media(&store, &cid_url, &bytes, &mime_type).await?;
    }
    Ok(())
}

/// Remove all object-store blobs owned by one session. Missing stores and
/// already-deleted objects are treated as an idempotent no-op.
///
/// Deprecated: Use `harnx_blob_store::delete_owner` instead.
pub async fn delete_session_attachments(
    jetstream: &jetstream::Context,
    session_id: &str,
) -> Result<usize> {
    harnx_blob_store::delete_owner(jetstream, session_id).await
}

/// Hydrate the attachment references currently present in a loaded session.
///
/// This is used during session preparation to download any referenced attachments
/// that are missing from the local cache.
async fn hydrate_session_attachments(
    jetstream: &jetstream::Context,
    config: &crate::config::GlobalConfig,
    replicas: usize,
    session: SessionRef,
) -> Result<()> {
    let (dir, refs) = {
        let config = config.read();
        let Some(sess) = config.session.as_ref() else {
            return Ok(());
        };
        let Some(dir) = crate::config::session_externalize::attachments_dir(sess) else {
            return Ok(());
        };
        (dir, collect_cid_refs(&sess.messages))
    };
    hydrate_attachment_refs(
        AttachmentLocation::new(jetstream, replicas, &session),
        &dir,
        &refs,
    )
    .await
}
