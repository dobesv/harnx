//! Session-owned uploads reuse the serving connection and broker chunk ceiling.

use async_nats::jetstream::{self, object_store::ObjectStore};
use harnx_blob_store::media::MediaBlob;
use harnx_core::{
    attachments::cid_for_data_url_with_session,
    cid_url::{CidUrl, SessionRef},
};
use harnx_toolset::ToolInvokeError;
use tokio::sync::OnceCell;

pub(super) struct Storage {
    jetstream: jetstream::Context,
    replicas: usize,
    bucket: OnceCell<ObjectStore>,
}

impl Storage {
    pub(super) fn new(jetstream: jetstream::Context, replicas: usize) -> Self {
        Self {
            jetstream,
            replicas,
            bucket: OnceCell::new(),
        }
    }

    pub(super) async fn uploads(
        &self,
        session: &SessionRef,
    ) -> Result<AttachmentUploads<'_>, ToolInvokeError> {
        let store = self
            .bucket
            .get_or_try_init(|| async {
                harnx_blob_store::create_or_open_attachments_bucket(&self.jetstream, self.replicas)
                    .await
            })
            .await
            .map_err(fatal)?;
        let _ = harnx_blob_store::touch_activity(&self.jetstream, &session.owner()).await;
        Ok(AttachmentUploads {
            store,
            // A valid broker ceiling can be below async-nats' 128 KiB default.
            max_chunk_bytes: self.jetstream.client().server_info().max_payload,
        })
    }
}

pub(super) struct AttachmentUploads<'a> {
    store: &'a ObjectStore,
    max_chunk_bytes: usize,
}

impl AttachmentUploads<'_> {
    pub(super) async fn put(&self, blob: MediaBlob<'_>) -> Result<(), ToolInvokeError> {
        harnx_blob_store::media::put_media_with_chunk_limit(self.store, blob, self.max_chunk_bytes)
            .await
            .map_err(fatal)
    }
}

pub(super) fn data_url_cid(session: &SessionRef, mime: &str, encoded: &str) -> CidUrl {
    CidUrl::parse(&cid_for_data_url_with_session(
        session,
        &format!("data:{mime};base64,{encoded}"),
    ))
    .expect("canonical media CID")
}

pub(super) fn fatal(error: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Fatal(format!("MCP result attachment storage failed: {error:#}"))
}
