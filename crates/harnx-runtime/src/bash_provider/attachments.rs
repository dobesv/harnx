//! Explicit-CID reads on the same broker connection as the round's tools.

use std::{io, sync::Arc};

use async_trait::async_trait;
use harnx_blob_store::{media::media_size, touch_activity};
use harnx_core::cid_url::CidUrl;
use harnx_script::{
    fs_attachments::{AttachmentFetcher, AttachmentFs},
    FsMount,
};

pub(crate) fn fetcher(jetstream: async_nats::jetstream::Context) -> Arc<dyn AttachmentFetcher> {
    Arc::new(NatsAttachmentFetcher {
        jetstream,
        bucket: std::sync::Mutex::new(None),
    })
}

pub(super) fn mount(fetcher: Option<Arc<dyn AttachmentFetcher>>) -> FsMount {
    AttachmentFs::new(fetcher.unwrap_or_else(|| Arc::new(Unavailable))).mount()
}

struct NatsAttachmentFetcher {
    jetstream: async_nats::jetstream::Context,
    /// Bucket handle memoized after the first successful open. Only positive
    /// results are memoized: an absent bucket is looked up again so a store
    /// created later in the round is still seen.
    bucket: std::sync::Mutex<Option<async_nats::jetstream::object_store::ObjectStore>>,
}

impl NatsAttachmentFetcher {
    async fn bucket(
        &self,
    ) -> anyhow::Result<Option<async_nats::jetstream::object_store::ObjectStore>> {
        if let Some(store) = self.bucket.lock().expect("bucket cache poisoned").clone() {
            return Ok(Some(store));
        }
        let store = harnx_blob_store::media::optional_attachments_bucket(&self.jetstream).await?;
        if let Some(store) = &store {
            *self.bucket.lock().expect("bucket cache poisoned") = Some(store.clone());
        }
        Ok(store)
    }

    /// Metadata-only size probe. Missing bucket or object is `Ok(None)` per
    /// the trait contract; only broker consults touch activity.
    async fn lookup(&self, cid: &str) -> io::Result<Option<u64>> {
        let url = parse_media(cid)?;
        let store = self.bucket().await.map_err(storage_error)?;
        let Some(store) = store else {
            return Ok(None);
        };
        let size = media_size(&store, &url).await.map_err(storage_error)?;
        if size.is_some() {
            if let Err(error) = touch_activity(&self.jetstream, &url.owner()).await {
                log::debug!("activity touch failed during script attachment read: {error:#}");
            }
        }
        Ok(size)
    }
}

#[async_trait]
impl AttachmentFetcher for NatsAttachmentFetcher {
    async fn fetch(&self, cid: &str, max_bytes: u64) -> io::Result<Vec<u8>> {
        let url = parse_media(cid)?;
        // Match attachment_read/resolve: explicit canonical CIDs on this broker,
        // including shared cross-session references. Storage owner keys are not
        // caller ACLs. Never enumerate or provision buckets from a script read.
        let store = self
            .bucket()
            .await
            .map_err(storage_error)?
            .ok_or_else(|| missing(cid))?;
        let (bytes, _) = harnx_blob_store::media::get_media_bounded(&store, &url, max_bytes)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| missing(cid))?;
        if let Err(error) = touch_activity(&self.jetstream, &url.owner()).await {
            log::debug!("activity touch failed during script attachment read: {error:#}");
        }
        Ok(bytes)
    }

    async fn size(&self, cid: &str) -> io::Result<Option<u64>> {
        self.lookup(cid).await
    }
}

fn parse_media(cid: &str) -> io::Result<CidUrl> {
    let url = CidUrl::parse(cid)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    if !matches!(url, CidUrl::Media { .. }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attachments mount requires a cid:media: URL",
        ));
    }
    Ok(url)
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
        Err(unavailable())
    }

    async fn size(&self, _cid: &str) -> io::Result<Option<u64>> {
        Err(unavailable())
    }
}

fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "attachment storage unavailable: this round has no NATS connection",
    )
}

#[cfg(test)]
mod tests;
