//! Replace inner media before any script output mode can observe inline bytes.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use harnx_core::{
    cid_url::{CidUrl, SessionRef},
    message::MessageContentPart,
};
use harnx_script::fs_attachments::attachment_path;
use serde_json::{json, Value};

use crate::nats_attachments::ExternalizedAttachment;

#[async_trait::async_trait]
pub(crate) trait MediaStore: Send + Sync {
    /// Return only after bytes are readable through this run's attachment fetcher.
    /// Successful writes may be shared by other calls, so later failures don't
    /// roll them back.
    async fn store(&self, data_url: &str) -> Result<ExternalizedAttachment>;
}

pub(crate) fn nats_store(
    jetstream: async_nats::jetstream::Context,
    owner: Option<SessionRef>,
    replicas: usize,
) -> Arc<dyn MediaStore> {
    Arc::new(NatsMediaStore {
        jetstream,
        owner,
        replicas,
        writes: tokio::sync::Mutex::new(()),
    })
}

struct NatsMediaStore {
    jetstream: async_nats::jetstream::Context,
    owner: Option<SessionRef>,
    replicas: usize,
    writes: tokio::sync::Mutex<()>,
}

#[async_trait::async_trait]
impl MediaStore for NatsMediaStore {
    async fn store(&self, data_url: &str) -> Result<ExternalizedAttachment> {
        let owner = self
            .owner
            .as_ref()
            .context("script media requires a valid agent/session identity")?;
        // put_media's existence check and write aren't atomic. Serialize writes
        // so duplicate inline results cannot replace chunks under a new reader.
        let _write = self.writes.lock().await;
        let store =
            harnx_blob_store::media::ensure_attachments_bucket(&self.jetstream, self.replicas)
                .await?;
        // Same content-derived IDs and upload path as NATS message externalization.
        // Await the object-store acknowledgement before publishing its VFS path.
        let attachment =
            crate::nats_attachments::externalize_data_url(&store, owner, data_url).await?;
        if let Err(error) = harnx_blob_store::touch_activity(&self.jetstream, &owner.owner()).await
        {
            log::debug!("activity touch failed during script media write: {error:#}");
        }
        Ok(attachment)
    }
}

impl super::InnerCallRun {
    pub(crate) fn with_media(mut self, store: Option<Arc<dyn MediaStore>>) -> Self {
        self.media = Some(InnerMedia::new(store));
        self
    }

    pub(super) async fn prepare_media(
        &self,
        evaluated: Result<harnx_engine::tool::EvaluatedInnerCall>,
    ) -> Result<harnx_engine::tool::EvaluatedInnerCall> {
        let mut evaluated = evaluated?;
        if let Some(media) = &self.media {
            if let Err(error) = media.prepare(&mut evaluated.result.output).await {
                if error.downcast_ref::<InvalidMedia>().is_none() {
                    return Err(error);
                }
                // Never attach the original output: even a failed projection
                // must not expose inline data through stderr or partial results.
                evaluated.result.output = json!({
                    "is_error": true, "error": error.to_string(),
                });
                evaluated.status = harnx_engine::tool::InnerToolCallStatus::ToolError;
            }
        }
        Ok(evaluated)
    }
}

pub(super) struct InnerMedia {
    store: Option<Arc<dyn MediaStore>>,
}

#[derive(Clone, Copy)]
enum Pass {
    Validate,
    Upload,
}

impl InnerMedia {
    pub(super) fn new(store: Option<Arc<dyn MediaStore>>) -> Self {
        Self { store }
    }

    pub(super) async fn prepare(&self, value: &mut Value) -> Result<()> {
        // Keep image-reference validation, but never retain a diagnostic copy
        // of the original output, which can contain inline bytes.
        harnx_engine::media::extract_image_parts(value)
            .map_err(|_| invalid("invalid tool image attachment reference"))?;
        // Both passes use the same walk, including JSON text mirrors. Reject
        // invalid later items before any upload or mutation of the result.
        self.rewrite(value, 0, Pass::Validate).await?;
        self.rewrite(value, 0, Pass::Upload).await?;
        Ok(())
    }

    fn rewrite<'a>(
        &'a self,
        value: &'a mut Value,
        depth: usize,
        pass: Pass,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            anyhow::ensure!(
                depth < 128,
                InvalidMedia("tool media nesting limit exceeded")
            );
            if let Some(data_url) = inline_data_url(value)? {
                return self.rewrite_block(value, &data_url, pass).await;
            }
            match value {
                Value::String(text) => self.rewrite_text(text, depth, pass).await,
                Value::Object(object) => {
                    self.rewrite_children(media_children(object), depth, pass)
                        .await
                }
                Value::Array(items) => self.rewrite_children(items.iter_mut(), depth, pass).await,
                _ => Ok(false),
            }
        })
    }

    async fn rewrite_children<'a>(
        &self,
        children: impl Iterator<Item = &'a mut Value> + Send,
        depth: usize,
        pass: Pass,
    ) -> Result<bool> {
        let mut changed = false;
        for child in children {
            changed |= self.rewrite(child, depth + 1, pass).await?;
        }
        Ok(changed)
    }

    async fn rewrite_block(&self, value: &mut Value, data_url: &str, pass: Pass) -> Result<bool> {
        let Some(attachment) = self.externalize(data_url, pass).await? else {
            return Ok(false);
        };
        let path = attachment_path(&attachment.cid)?;
        let text = format!("{} path={}", attachment.marker(), path.display());
        *value = json!({
            "type": "text", "text": text, "uri": attachment.cid,
            "mimeType": attachment.mime_type, "size": attachment.size, "path": path,
        });
        Ok(true)
    }

    async fn rewrite_text(&self, text: &mut String, depth: usize, pass: Pass) -> Result<bool> {
        if is_base64_data_uri(text) {
            let Some(attachment) = self.externalize(text, pass).await? else {
                return Ok(false);
            };
            *text = format!(
                "{} path={}",
                attachment.marker(),
                attachment_path(&attachment.cid)?.display()
            );
            return Ok(true);
        }
        // MCP can mirror structuredContent as JSON text. Parse typed media
        // even when the JSON contains no data: URI, such as an image block.
        let Some(mut mirrored) = json_mirror(text) else {
            return Ok(false);
        };
        let changed = self.rewrite(&mut mirrored, depth + 1, pass).await?;
        if changed {
            *text = mirrored.to_string();
        }
        Ok(changed)
    }

    async fn externalize(
        &self,
        data_url: &str,
        pass: Pass,
    ) -> Result<Option<ExternalizedAttachment>> {
        if matches!(pass, Pass::Validate) {
            validate_data_url(data_url)?;
            return Ok(None);
        }
        let attachment = self
            .store
            .as_ref()
            .context("attachment storage unavailable for inline script media")?
            .store(data_url)
            .await
            .context("externalize inner tool media")?;
        Ok(Some(attachment))
    }
}

fn validate_data_url(data_url: &str) -> Result<()> {
    let max_bytes = harnx_script::FsLimits::default().max_file_size;
    let encoded = data_url
        .split_once(";base64,")
        .map(|(_, encoded)| encoded)
        .ok_or_else(|| invalid("inline media requires a base64 data URI"))?;
    anyhow::ensure!(
        encoded.len() as u64 <= max_bytes.div_ceil(3) * 4,
        InvalidMedia("inline media exceeds the attachment file-size limit")
    );
    let (_, bytes) = crate::nats_attachments::parse_data_url(data_url)
        .map_err(|_| invalid("invalid inline media data URI or base64"))?;
    anyhow::ensure!(
        bytes.len() as u64 <= max_bytes,
        InvalidMedia("inline media exceeds the attachment file-size limit")
    );
    Ok(())
}

fn is_base64_data_uri(text: &str) -> bool {
    text.strip_prefix("data:")
        .is_some_and(|body| body.contains(";base64,"))
}

fn json_mirror(text: &str) -> Option<Value> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(text).ok();
    }
    None
}

fn media_children(object: &mut serde_json::Map<String, Value>) -> impl Iterator<Item = &mut Value> {
    let text_block = object.get("type").and_then(Value::as_str) == Some("text");
    object.iter_mut().filter_map(move |(key, child)| {
        if text_block && key == "text" {
            // Literal text is not an inline attachment. JSON text mirrors still
            // pass through the walker to find typed image/audio/resource blocks.
            if child.as_str().is_some_and(|text| text.starts_with("data:")) {
                return None;
            }
        }
        Some(child)
    })
}

fn inline_data_url(value: &Value) -> Result<Option<String>> {
    match value.get("type").and_then(Value::as_str) {
        Some("image") => image_data_url(value),
        Some("audio") => audio_data_url(value),
        Some("resource") => resource_data_url(value),
        Some("resource_link") => resource_uri_data_url(value),
        None => unwrapped_data_url(value),
        _ => Ok(None),
    }
}

fn unwrapped_data_url(value: &Value) -> Result<Option<String>> {
    // Structured results may carry a resource body without the MCP wrapper.
    // Require a MIME type before treating an otherwise ordinary data field
    // as media; don't guess that every business string is base64.
    if let Some(mime) = mime_type(value) {
        let kind = mime.split('/').next().unwrap_or("").trim();
        let data = value.get("blob").or_else(|| {
            (kind.eq_ignore_ascii_case("image") || kind.eq_ignore_ascii_case("audio"))
                .then(|| value.get("data"))
                .flatten()
        });
        if let Some(data) = data {
            let data = data
                .as_str()
                .ok_or_else(|| invalid("inline media body must be a string"))?;
            return Ok(Some(format!("data:{mime};base64,{data}")));
        }
    }
    Ok(None)
}

fn image_data_url(value: &Value) -> Result<Option<String>> {
    let mut images = harnx_engine::media::extract_image_parts(&json!({"content": [value]}))?;
    match images.pop() {
        Some(MessageContentPart::ImageUrl { image_url }) => Ok(Some(image_url.url)),
        _ => Err(invalid("inline image requires string data and MIME type")),
    }
}

fn audio_data_url(value: &Value) -> Result<Option<String>> {
    let mime = mime_type(value).ok_or_else(|| invalid("inline audio requires a MIME type"))?;
    let data = value
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("inline audio requires base64 data"))?;
    Ok(Some(format!("data:{mime};base64,{data}")))
}

fn resource_data_url(value: &Value) -> Result<Option<String>> {
    let resource = value
        .get("resource")
        .context(InvalidMedia("embedded resource is missing its body"))?;
    let mime = mime_type(resource).unwrap_or("application/octet-stream");
    if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
        Ok(Some(format!("data:{mime};base64,{blob}")))
    } else if let Some(text) = resource.get("text").and_then(Value::as_str) {
        Ok(Some(format!(
            "data:{mime};base64,{}",
            harnx_core::crypto::base64_encode(text.as_bytes())
        )))
    } else if resource.get("blob").is_some() || resource.get("text").is_some() {
        Err(invalid("embedded resource body must be a string"))
    } else {
        resource_uri_data_url(resource)
    }
}

fn resource_uri_data_url(value: &Value) -> Result<Option<String>> {
    if let Some(uri) = value.get("uri").and_then(Value::as_str) {
        if is_base64_data_uri(uri) {
            return Ok(Some(uri.to_owned()));
        }
        if uri.starts_with("cid:media:") {
            CidUrl::parse(uri).map_err(|_| invalid("invalid media attachment reference"))?;
        }
    }
    Ok(None)
}

fn mime_type(value: &Value) -> Option<&str> {
    value
        .get("mimeType")
        .or_else(|| value.get("mime_type"))
        .and_then(Value::as_str)
}

#[derive(Debug)]
pub(super) struct InvalidMedia(&'static str);

impl std::fmt::Display for InvalidMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidMedia {}

fn invalid(message: &'static str) -> anyhow::Error {
    InvalidMedia(message).into()
}

#[cfg(test)]
mod tests;
