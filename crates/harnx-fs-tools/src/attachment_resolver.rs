//! Optional CID resolution for filesystem servers hosted over NATS.

use async_trait::async_trait;
use harnx_blob_store::BlobTooLarge;
use harnx_core::cid_url::CidUrl;
use std::fmt;

/// Resolve attachment bytes or rendered plan Markdown without decoding the bytes.
#[async_trait]
pub trait AttachmentResolver: Send + Sync {
    /// Reject invalid URLs and content larger than `max_bytes`.
    async fn resolve(&self, url: &str, max_bytes: usize)
        -> Result<Vec<u8>, AttachmentResolveError>;
}

/// Failures callers can turn into recoverable tool errors.
#[derive(Debug)]
pub enum AttachmentResolveError {
    InvalidUrl(anyhow::Error),
    NotFound(anyhow::Error),
    TooLarge(BlobTooLarge),
    Other(anyhow::Error),
}

impl fmt::Display for AttachmentResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(error) => write!(f, "Invalid attachment URL: {error:#}"),
            Self::NotFound(error) => write!(f, "{error:#}"),
            Self::TooLarge(error) => error.fmt(f),
            Self::Other(error) => write!(f, "Attachment resolution failed: {error:#}"),
        }
    }
}

impl std::error::Error for AttachmentResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::InvalidUrl(error) | Self::NotFound(error) | Self::Other(error) => error.as_ref(),
            Self::TooLarge(error) => error,
        })
    }
}

/// Uses the hosting server's JetStream context; never opens a NATS connection.
#[derive(Clone)]
pub struct NatsAttachmentResolver {
    jetstream: async_nats::jetstream::Context,
}

impl NatsAttachmentResolver {
    pub fn new(jetstream: async_nats::jetstream::Context) -> Self {
        Self { jetstream }
    }
}

#[async_trait]
impl AttachmentResolver for NatsAttachmentResolver {
    async fn resolve(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, AttachmentResolveError> {
        let url = CidUrl::parse(url).map_err(AttachmentResolveError::InvalidUrl)?;
        harnx_blob_store::resolve_bounded(&self.jetstream, &url, max_bytes)
            .await
            .map(|blob| blob.bytes)
            .map_err(|error| classify_resolution_error(&url, error))
    }
}

fn classify_resolution_error(url: &CidUrl, error: anyhow::Error) -> AttachmentResolveError {
    if let Some(too_large) = error.downcast_ref::<BlobTooLarge>() {
        return AttachmentResolveError::TooLarge(*too_large);
    }
    // Blob-store missing-resource errors aren't typed. Match only its exact
    // messages so transport and storage failures remain Other.
    let not_found = match url {
        CidUrl::Media { .. } => format!("attachment not found: {url}"),
        CidUrl::Plan { .. } => format!("plan document not found: {url}"),
    };
    if error.chain().any(|cause| cause.to_string() == not_found) {
        AttachmentResolveError::NotFound(error)
    } else {
        AttachmentResolveError::Other(error)
    }
}

#[cfg(test)]
mod tests;
