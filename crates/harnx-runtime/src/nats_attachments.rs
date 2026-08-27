//! Durable attachment blobs for NATS-backed sessions.
//!
//! Session logs keep only content-addressed `cid:` references. The matching
//! bytes live in one JetStream object store under a session-scoped object
//! name, so payloads stay below NATS message limits and session deletion can
//! garbage-collect exactly the blobs owned by that session.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, object_store, stream};
use futures_util::StreamExt;
use harnx_core::attachments::{
    collect_cid_refs, read_attachment_async, store_attachment_bytes_async,
};
use harnx_core::message::{MessageContent, MessageContentPart};
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncReadExt;

/// JetStream object-store bucket containing durable session attachment blobs.
pub const SESSION_ATTACHMENTS_BUCKET: &str = "harnx_attachments";
const CONTENT_TYPE_METADATA_KEY: &str = "content_type";

#[derive(Clone, Copy)]
pub struct AttachmentLocation<'a> {
    jetstream: &'a jetstream::Context,
    replicas: usize,
    session_id: &'a str,
}

impl<'a> AttachmentLocation<'a> {
    /// Describe the object-store location for one session's attachments.
    pub fn new(jetstream: &'a jetstream::Context, replicas: usize, session_id: &'a str) -> Self {
        Self {
            jetstream,
            replicas,
            session_id,
        }
    }
}

struct AttachmentPayload<'a> {
    cid: &'a str,
    mime_type: &'a str,
    bytes: &'a [u8],
}

/// Worker-side attachment lifecycle for one NATS session activation.
pub(crate) struct SessionAttachmentSync {
    jetstream: jetstream::Context,
    config: crate::config::GlobalConfig,
    replicas: usize,
    session_id: String,
}

impl SessionAttachmentSync {
    pub(crate) async fn prepare(
        jetstream: jetstream::Context,
        config: crate::config::GlobalConfig,
        cluster_key: &str,
        session_id: &str,
    ) -> Result<Self> {
        let config_snapshot = config.read().clone();
        let replicas = config_snapshot
            .resolve_nats_server(cluster_key)
            .await?
            .resolved_replicas();
        hydrate_session_attachments(&jetstream, &config, replicas, session_id).await?;
        Ok(Self {
            jetstream,
            config,
            replicas,
            session_id: session_id.to_string(),
        })
    }

    pub(crate) async fn finish<T>(self, result: Result<T>) -> Result<T> {
        let attachment_sync = sync_session_attachments(
            &self.jetstream,
            &self.config,
            self.replicas,
            &self.session_id,
        )
        .await;
        match result {
            Ok(value) => {
                attachment_sync?;
                Ok(value)
            }
            Err(error) => {
                if let Err(sync_error) = attachment_sync {
                    log::warn!(
                        "failed to sync session attachments after turn error: session_id={} error={sync_error:#}",
                        self.session_id
                    );
                }
                Err(error)
            }
        }
    }
}

fn object_store_stream_name() -> String {
    format!("OBJ_{SESSION_ATTACHMENTS_BUCKET}")
}

fn session_object_prefix(session_id: &str) -> String {
    format!("{}/", harnx_core::crypto::sha256(session_id))
}

fn attachment_object_name(session_id: &str, cid: &str) -> String {
    format!(
        "{}{}",
        session_object_prefix(session_id),
        harnx_core::crypto::sha256(cid)
    )
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

async fn ensure_store(
    jetstream: &jetstream::Context,
    replicas: usize,
) -> Result<object_store::ObjectStore> {
    let create = jetstream
        .create_object_store(object_store::Config {
            bucket: SESSION_ATTACHMENTS_BUCKET.to_string(),
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
            "could not reconcile replicas for attachment object store '{SESSION_ATTACHMENTS_BUCKET}': {error:#}"
        );
    }
    jetstream
        .get_object_store(SESSION_ATTACHMENTS_BUCKET)
        .await
        .map_err(anyhow::Error::from)
        .context("open NATS session attachment object store")
}

async fn optional_store(
    jetstream: &jetstream::Context,
) -> Result<Option<object_store::ObjectStore>> {
    match jetstream.get_stream(object_store_stream_name()).await {
        Ok(_) => jetstream
            .get_object_store(SESSION_ATTACHMENTS_BUCKET)
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

async fn put_attachment(
    store: &object_store::ObjectStore,
    session_id: &str,
    attachment: AttachmentPayload<'_>,
) -> Result<()> {
    let AttachmentPayload {
        cid,
        mime_type,
        bytes,
    } = attachment;
    let name = attachment_object_name(session_id, cid);
    if store.info(&name).await.is_ok() {
        return Ok(());
    }
    let mut metadata = HashMap::new();
    metadata.insert(CONTENT_TYPE_METADATA_KEY.to_string(), mime_type.to_string());
    let object = object_store::ObjectMetadata {
        name,
        description: Some(format!("Attachment {cid} for Harnx session {session_id}")),
        metadata,
        ..Default::default()
    };
    let mut reader = bytes;
    store
        .put(object, &mut reader)
        .await
        .with_context(|| format!("upload attachment {cid} for session '{session_id}'"))?;
    Ok(())
}

async fn externalize_part(
    store: &object_store::ObjectStore,
    session_id: &str,
    source_dir: Option<&Path>,
    part: &mut MessageContentPart,
) -> Result<()> {
    let MessageContentPart::ImageUrl { image_url } = part else {
        return Ok(());
    };
    let (cid, mime_type, bytes) = if image_url.url.starts_with("data:") {
        let (mime_type, bytes) = parse_data_url(&image_url.url)?;
        let cid = harnx_core::attachments::cid_for_data_url(&image_url.url);
        (cid, mime_type, bytes)
    } else if image_url
        .url
        .starts_with(harnx_core::attachments::CID_PREFIX)
    {
        let cid = image_url.url.clone();
        if store
            .info(&attachment_object_name(session_id, &cid))
            .await
            .is_ok()
        {
            return Ok(());
        }
        let source_dir = source_dir.with_context(|| {
            format!("attachment {} has no local source directory", image_url.url)
        })?;
        let (bytes, mime_type) = read_attachment_async(source_dir, &cid).await?;
        (cid, mime_type, bytes)
    } else {
        return Ok(());
    };
    put_attachment(
        store,
        session_id,
        AttachmentPayload {
            cid: &cid,
            mime_type: &mime_type,
            bytes: &bytes,
        },
    )
    .await?;
    image_url.url = cid;
    Ok(())
}

/// Upload inline or locally-referenced attachments and rewrite inline data
/// URIs to durable `cid:` references suitable for the session log.
pub async fn externalize_message_attachments(
    location: AttachmentLocation<'_>,
    content: &mut MessageContent,
    source_dir: Option<&Path>,
) -> Result<()> {
    let MessageContent::Array(parts) = content else {
        return Ok(());
    };
    let needs_store = parts.iter().any(|part| {
        matches!(part, MessageContentPart::ImageUrl { image_url }
            if image_url.url.starts_with("data:")
                || image_url.url.starts_with(harnx_core::attachments::CID_PREFIX))
    });
    if !needs_store {
        return Ok(());
    }
    let store = ensure_store(location.jetstream, location.replicas).await?;
    for part in parts {
        externalize_part(&store, location.session_id, source_dir, part).await?;
    }
    Ok(())
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
    let store = match optional_store(location.jetstream).await? {
        Some(store) => store,
        None => ensure_store(location.jetstream, location.replicas).await?,
    };
    let hydration = AttachmentHydration {
        store: &store,
        session_id: location.session_id,
        dir,
    };
    for cid in refs {
        hydrate_attachment_ref(&hydration, cid).await?;
    }
    Ok(())
}

struct AttachmentHydration<'a> {
    store: &'a object_store::ObjectStore,
    session_id: &'a str,
    dir: &'a Path,
}

async fn hydrate_attachment_ref(hydration: &AttachmentHydration<'_>, cid: &str) -> Result<()> {
    let name = attachment_object_name(hydration.session_id, cid);
    let local_attachment = read_attachment_async(hydration.dir, cid).await.ok();
    let stored_in_nats = hydration.store.info(&name).await.is_ok();
    if let Some((bytes, mime_type)) = local_attachment {
        if stored_in_nats {
            return Ok(());
        }
        return put_attachment(
            hydration.store,
            hydration.session_id,
            AttachmentPayload {
                cid,
                mime_type: &mime_type,
                bytes: &bytes,
            },
        )
        .await;
    }
    let mut object = hydration.store.get(&name).await.with_context(|| {
        format!(
            "download attachment {cid} for session '{}'",
            hydration.session_id
        )
    })?;
    let mime_type = object
        .info()
        .metadata
        .get(CONTENT_TYPE_METADATA_KEY)
        .cloned()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let mut bytes = Vec::new();
    object.read_to_end(&mut bytes).await.with_context(|| {
        format!(
            "read attachment {cid} for session '{}'",
            hydration.session_id
        )
    })?;
    let stored_cid = store_attachment_bytes_async(hydration.dir, &bytes, &mime_type).await?;
    if stored_cid != cid {
        anyhow::bail!(
            "attachment digest mismatch for session '{}': expected {cid}, got {stored_cid}",
            hydration.session_id
        );
    }
    Ok(())
}

/// Hydrate the attachment references currently present in a loaded session.
pub async fn hydrate_session_attachments(
    jetstream: &jetstream::Context,
    config: &crate::config::GlobalConfig,
    replicas: usize,
    session_id: &str,
) -> Result<()> {
    let (dir, refs) = {
        let config = config.read();
        let Some(session) = config.session.as_ref() else {
            return Ok(());
        };
        let Some(dir) = crate::config::session_externalize::attachments_dir(session) else {
            return Ok(());
        };
        (dir, collect_cid_refs(&session.messages))
    };
    hydrate_attachment_refs(
        AttachmentLocation::new(jetstream, replicas, session_id),
        &dir,
        &refs,
    )
    .await
}

/// Upload locally-created session attachments, such as image content returned
/// by tools, before the worker publishes the durable turn boundary.
pub async fn sync_session_attachments(
    jetstream: &jetstream::Context,
    config: &crate::config::GlobalConfig,
    replicas: usize,
    session_id: &str,
) -> Result<()> {
    let (dir, refs) = {
        let config = config.read();
        let Some(session) = config.session.as_ref() else {
            return Ok(());
        };
        let Some(dir) = crate::config::session_externalize::attachments_dir(session) else {
            return Ok(());
        };
        (dir, collect_cid_refs(&session.messages))
    };
    if refs.is_empty() {
        return Ok(());
    }
    let store = ensure_store(jetstream, replicas).await?;
    for cid in refs {
        let (bytes, mime_type) = read_attachment_async(&dir, &cid).await?;
        put_attachment(
            &store,
            session_id,
            AttachmentPayload {
                cid: &cid,
                mime_type: &mime_type,
                bytes: &bytes,
            },
        )
        .await?;
    }
    Ok(())
}

/// Remove all object-store blobs owned by one session. Missing stores and
/// already-deleted objects are treated as an idempotent no-op.
pub async fn delete_session_attachments(
    jetstream: &jetstream::Context,
    session_id: &str,
) -> Result<usize> {
    let Some(store) = optional_store(jetstream).await? else {
        return Ok(0);
    };
    let prefix = session_object_prefix(session_id);
    let mut objects = store
        .list()
        .await
        .context("list NATS session attachments")?;
    let mut names = Vec::new();
    while let Some(info) = objects.next().await {
        let info = info.context("list NATS session attachment metadata")?;
        if info.name.starts_with(&prefix) {
            names.push(info.name);
        }
    }
    for name in &names {
        store
            .delete(name)
            .await
            .with_context(|| format!("delete NATS session attachment '{name}'"))?;
    }
    Ok(names.len())
}
