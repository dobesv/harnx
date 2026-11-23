use std::path::Path;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use harnx_blob_store::media::AttachmentsBucket;
use harnx_core::attachments::{read_attachment_async, store_attachment_bytes_async, CID_PREFIX};
use harnx_core::cid_url::{CidUrl, SessionRef};
use http::StatusCode;
use multer::Multipart;
use serde_json::json;

use crate::{
    is_safe_agent_path, is_safe_path_segment, nats_access::is_nats_connect_error, percent_decode,
    session_actor::ResolvedAgentTarget, Server, MAX_UPLOAD_BYTES,
};

/// Store the blob locally and return its identity: the same `CidUrl` the
/// response ref is built from, whose `kv_key()` is also the object-store key
/// the blob is published under.
async fn save_attachment_file(
    attachments_dir: &Path,
    attachment_session: &SessionRef,
    data: &[u8],
    mime: &str,
) -> Result<CidUrl> {
    let stored_cid = store_attachment_bytes_async(attachments_dir, data, mime).await?;
    let hash = stored_cid
        .strip_prefix(CID_PREFIX)
        .context("stored attachment cid is missing cid prefix")?;
    Ok(CidUrl::Media {
        session: attachment_session.clone(),
        hash: hash.to_string(),
    })
}

/// One upload request's lazy handle on the shared attachments object store.
///
/// JetStream is only taken when the first valid part is stored, so a request
/// rejected by validation (unsupported MIME type, oversized part, bad
/// multipart framing) never needs a broker and keeps its 4xx even when the
/// broker is down.
pub(super) struct AttachmentObjectStore<'a> {
    server: &'a Server,
    target: &'a ResolvedAgentTarget,
    bucket: Option<AttachmentsBucket>,
}

impl<'a> AttachmentObjectStore<'a> {
    pub(super) fn new(server: &'a Server, target: &'a ResolvedAgentTarget) -> Self {
        Self {
            server,
            target,
            bucket: None,
        }
    }

    /// Write one accepted blob to the object store. The first call takes
    /// JetStream and ensures the bucket; later parts reuse both.
    pub(super) async fn put(&mut self, url: &CidUrl, bytes: &[u8], mime: &str) -> Result<()> {
        if self.bucket.is_none() {
            let bucket = self.server.attachments_bucket(self.target).await?;
            self.bucket = Some(bucket);
        }
        let bucket = self
            .bucket
            .as_ref()
            .context("attachment bucket was ensured for this upload")?;
        harnx_blob_store::media::put_media(bucket, url, bytes, mime).await
    }
}

/// Object-store failures must not surface as the route's default 400, and the
/// response must carry no refs. Connection and unavailability failures are
/// 503 (the store is currently unreachable); any other store failure is 500.
/// Both keep the `{"error": ...}` body shape the web UI reads.
fn attachment_store_error(error: anyhow::Error) -> AttachmentUploadError {
    log::error!("failed to store uploaded attachment in the NATS object store: {error:#}");
    // `sanitize_nats_cluster_error` collapses remote connection failures to
    // this credential-free message.
    let unavailable = is_nats_connect_error(&error)
        || error.chain().any(|cause| {
            cause.to_string().starts_with("NATS cluster '")
                && cause.to_string().ends_with("is unavailable")
        });
    if unavailable {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "attachment object store is unavailable"}),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": format!("failed to store attachment in the object store: {error:#}")}),
        )
    }
}

pub(super) async fn read_cached_or_nats_attachment(
    attachments_dir: &Path,
    local_cid: &str,
    jetstream: &async_nats::jetstream::Context,
    cid: &str,
) -> Result<Option<(Vec<u8>, String)>> {
    match read_attachment_async(attachments_dir, local_cid).await {
        Ok(attachment) => Ok(Some(attachment)),
        Err(_) => {
            Ok(harnx_runtime::nats_attachments::get_session_attachment(jetstream, 1, cid).await?)
        }
    }
}

pub(super) type AttachmentUploadError = (StatusCode, serde_json::Value);

async fn process_field(
    field: multer::Field<'_>,
    attachments_dir: &Path,
    attachment_session: &SessionRef,
    object_store: &mut AttachmentObjectStore<'_>,
    refs: &mut Vec<String>,
) -> Result<Option<AttachmentUploadError>> {
    let name = field.name().unwrap_or_default().to_string();
    if !matches!(name.as_str(), "attachment" | "attachments" | "file") {
        return Ok(None);
    }

    let mime = field
        .content_type()
        .map(|mime| mime.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "application/pdf" | "text/plain"
    ) {
        return Ok(Some((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            json!({
                "error": "unsupported attachment content type",
                "field": name,
                "content_type": mime,
            }),
        )));
    }

    let data = field
        .bytes()
        .await
        .map_err(|err| anyhow!("Bad Request: {err}"))?;
    if data.len() > MAX_UPLOAD_BYTES {
        return Ok(Some((
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error":"attachment too large","max_bytes":MAX_UPLOAD_BYTES}),
        )));
    }

    // Local write first: it is the content-addressed warm cache, and the hash
    // the returned ref is built from comes from this store. A NATS failure may
    // leave this file behind; that is a stale cache entry, not a correctness
    // problem.
    let url = save_attachment_file(attachments_dir, attachment_session, &data, &mime).await?;
    if let Err(error) = object_store.put(&url, &data, &mime).await {
        return Ok(Some(attachment_store_error(error)));
    }
    refs.push(url.to_string());
    Ok(None)
}

pub(super) async fn process_multipart_upload(
    body_bytes: Vec<u8>,
    boundary: String,
    attachments_dir: &Path,
    attachment_session: &SessionRef,
    object_store: &mut AttachmentObjectStore<'_>,
) -> Result<Result<Vec<String>, AttachmentUploadError>> {
    let stream =
        futures_util::stream::once(
            async move { Ok::<Bytes, std::io::Error>(Bytes::from(body_bytes)) },
        );
    let mut multipart = Multipart::new(stream, boundary);
    let mut refs = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| anyhow!("Bad Request: {err}"))?
    {
        if let Some(error) = process_field(
            field,
            attachments_dir,
            attachment_session,
            object_store,
            &mut refs,
        )
        .await?
        {
            return Ok(Err(error));
        }
    }

    if refs.is_empty() {
        return Ok(Err((
            StatusCode::BAD_REQUEST,
            json!({"error":"no attachment parts found"}),
        )));
    }
    Ok(Ok(refs))
}

fn parse_agent_session_segments(path: &str) -> Option<(String, String, Vec<&str>)> {
    let suffix = path.strip_prefix("/v1/agents/")?;
    let segments: Vec<_> = suffix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let [agent, "sessions", session, remaining @ ..] = segments.as_slice() else {
        return None;
    };

    let agent = percent_decode(agent);
    let session = percent_decode(session);
    if !is_safe_agent_path(&agent) || !is_safe_path_segment(&session) {
        return None;
    }
    Some((agent, session, remaining.to_vec()))
}

pub(super) fn parse_session_attachments_path(path: &str) -> Option<(String, String)> {
    let (agent, session, remaining) = parse_agent_session_segments(path)?;
    match remaining.as_slice() {
        ["attachments"] => Some((agent, session)),
        _ => None,
    }
}

pub(super) fn parse_session_attachment_blob_path(path: &str) -> Option<(String, String, String)> {
    let (agent, session, remaining) = parse_agent_session_segments(path)?;
    match remaining.as_slice() {
        ["attachments", cid] => Some((agent, session, percent_decode(cid))),
        _ => None,
    }
}

pub(super) fn is_canonical_attachment_cid(cid: &[u8]) -> bool {
    std::str::from_utf8(cid)
        .ok()
        .and_then(|cid| CidUrl::parse(cid).ok())
        .is_some_and(|url| matches!(url, CidUrl::Media { .. }))
}

pub(super) fn is_inline_image_mime(mime_type: &[u8]) -> bool {
    matches!(
        mime_type,
        b"image/png" | b"image/jpeg" | b"image/webp" | b"image/gif"
    )
}

pub(super) fn request_is_oversized(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_UPLOAD_BYTES)
}
