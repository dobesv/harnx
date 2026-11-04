//! Explicit-CID reads on the same broker connection as the round's tools.

use std::{io, sync::Arc};

use async_trait::async_trait;
use harnx_blob_store::{
    media::{get_media_bounded, optional_attachments_bucket},
    touch_activity,
};
use harnx_core::cid_url::CidUrl;
use harnx_script::{
    fs_attachments::{AttachmentFetcher, AttachmentFs},
    FsMount,
};

pub(crate) fn fetcher(jetstream: async_nats::jetstream::Context) -> Arc<dyn AttachmentFetcher> {
    Arc::new(NatsAttachmentFetcher(jetstream))
}

pub(super) fn mount(fetcher: Option<Arc<dyn AttachmentFetcher>>) -> FsMount {
    AttachmentFs::new(fetcher.unwrap_or_else(|| Arc::new(Unavailable))).mount()
}

struct NatsAttachmentFetcher(async_nats::jetstream::Context);

#[async_trait]
impl AttachmentFetcher for NatsAttachmentFetcher {
    async fn fetch(&self, cid: &str, max_bytes: u64) -> io::Result<Vec<u8>> {
        let url = CidUrl::parse(cid)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        if !matches!(url, CidUrl::Media { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attachments mount requires a cid:media: URL",
            ));
        }
        // Match attachment_read/resolve: explicit canonical CIDs on this broker,
        // including shared cross-session references. Storage owner keys are not
        // caller ACLs. Never enumerate or provision buckets from a script read.
        let store = optional_attachments_bucket(&self.0)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| missing(cid))?;
        let (bytes, _) = get_media_bounded(&store, &url, max_bytes)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| missing(cid))?;
        if let Err(error) = touch_activity(&self.0, &url.owner()).await {
            log::debug!("activity touch failed during script attachment read: {error:#}");
        }
        Ok(bytes)
    }
}

fn missing(cid: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("attachment not found: {cid}"),
    )
}

fn storage_error(error: anyhow::Error) -> io::Error {
    io::Error::other(format!("read attachment: {error:#}"))
}

struct Unavailable;

#[async_trait]
impl AttachmentFetcher for Unavailable {
    async fn fetch(&self, _cid: &str, _max_bytes: u64) -> io::Result<Vec<u8>> {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "attachment storage unavailable: this round has no NATS connection",
        ))
    }
}

#[cfg(test)]
mod tests;
