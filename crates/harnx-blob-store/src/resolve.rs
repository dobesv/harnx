//! Resolution of canonical `cid:` URLs into bytes and response metadata.

use anyhow::{Context, Result};
use harnx_core::cid_url::CidUrl;

use crate::{
    limits::check_size,
    media::{get_media_with_limit, optional_attachments_bucket},
};

/// A resolved blob and its cache metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBlob {
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub etag: Option<String>,
    pub immutable: bool,
}

/// Resolve a canonical `cid:` URL from NATS-backed storage.
///
/// A read leaves the owning session's activity record alone. Session GC ages a
/// session by its activity, which turns and writes refresh, so a session that
/// is only read still expires.
pub async fn resolve(
    jetstream: &async_nats::jetstream::Context,
    url: &CidUrl,
) -> Result<ResolvedBlob> {
    resolve_with_limit(jetstream, url, None).await
}

/// Resolve a canonical `cid:` URL, rejecting blobs larger than `max_bytes`.
///
/// Media metadata is checked before reading, and the read stops after at most
/// `max_bytes + 1` bytes. Plan Markdown is checked after rendering. Size failures
/// can be identified by downcasting the error to [`crate::BlobTooLarge`].
pub async fn resolve_bounded(
    jetstream: &async_nats::jetstream::Context,
    url: &CidUrl,
    max_bytes: usize,
) -> Result<ResolvedBlob> {
    resolve_with_limit(jetstream, url, Some(max_bytes)).await
}

async fn resolve_with_limit(
    jetstream: &async_nats::jetstream::Context,
    url: &CidUrl,
    max_bytes: Option<usize>,
) -> Result<ResolvedBlob> {
    let resolved = match url {
        CidUrl::Media { hash, .. } => {
            let store = optional_attachments_bucket(jetstream)
                .await?
                .with_context(|| format!("attachment not found: {url}"))?;
            let (bytes, mime_type) = get_media_with_limit(&store, url, max_bytes)
                .await?
                .with_context(|| format!("attachment not found: {url}"))?;
            ResolvedBlob {
                mime_type,
                bytes,
                etag: Some(hash.clone()),
                immutable: true,
            }
        }
        CidUrl::Plan { .. } => {
            let rendered = crate::plans::render(jetstream, url).await?;
            check_size(rendered.markdown.len(), max_bytes)?;
            ResolvedBlob {
                mime_type: "text/markdown; charset=utf-8".to_string(),
                bytes: rendered.markdown.into_bytes(),
                etag: Some(rendered.max_revision.to_string()),
                immutable: false,
            }
        }
    };
    Ok(resolved)
}
