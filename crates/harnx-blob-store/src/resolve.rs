//! Resolution of canonical `cid:` URLs into bytes and response metadata.

use anyhow::{Context, Result};
use harnx_core::cid_url::CidUrl;

use crate::{get_media, media::optional_attachments_bucket, touch_activity};

/// A resolved blob and its cache metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBlob {
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub etag: Option<String>,
    pub immutable: bool,
}

/// Resolve a canonical `cid:` URL from NATS-backed storage.
pub async fn resolve(
    jetstream: &async_nats::jetstream::Context,
    url: &CidUrl,
) -> Result<ResolvedBlob> {
    let resolved = match url {
        CidUrl::Media { hash, .. } => {
            let store = optional_attachments_bucket(jetstream)
                .await?
                .with_context(|| format!("attachment not found: {url}"))?;
            let (bytes, mime_type) = get_media(&store, url)
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
            ResolvedBlob {
                mime_type: "text/markdown; charset=utf-8".to_string(),
                bytes: rendered.markdown.into_bytes(),
                etag: Some(rendered.max_revision.to_string()),
                immutable: false,
            }
        }
    };

    if let Err(error) = touch_activity(jetstream, &url.owner()).await {
        log::debug!("activity touch failed during resolve (non-fatal): {error:#}");
    }
    Ok(resolved)
}
