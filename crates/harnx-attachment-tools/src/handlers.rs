//! Attachment tool handlers.
//!
//! Implements `attachment_read` and `attachment_create` operations over NATS.

use anyhow::{Context, Result};
use async_nats::jetstream::object_store::ObjectStore;
use harnx_blob_store::{get_media, media_cid_url, put_media};
use harnx_core::cid_url::CidUrl;
use harnx_core::crypto::hex_encode;
use harnx_core::safety::{format_size, truncate_output, TruncateOpts};
use harnx_toolset_server::content::WithAudience;
use rmcp::model::{CallToolResult, ContentBlock, Role};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024; // 5 MB

/// Image MIME types that return as image content blocks.
const IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Text-like MIME types that return as truncated text.
const TEXT_MIME_TYPES: &[&str] = &[
    "text/plain",
    "text/markdown",
    "text/html",
    "text/csv",
    "application/json",
    "application/xml",
    "application/yaml",
    "application/x-yaml",
];

/// Parameters for `attachment_read`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AttachmentReadParams {
    /// The cid: URL to read.
    pub url: String,
    /// Start reading at this line number (1-indexed).
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to return from offset.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Return only the first N lines.
    #[serde(default)]
    pub head_lines: Option<usize>,
    /// Return only the last N lines.
    #[serde(default)]
    pub tail_lines: Option<usize>,
    /// Maximum output bytes.
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    /// Filter lines by regex pattern.
    #[serde(default)]
    pub grep: Option<String>,
}

/// Parameters for `attachment_create`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AttachmentCreateParams {
    /// The content to store.
    pub content: String,
    /// MIME type of the content (text types only).
    pub mime_type: String,
}

/// Read an attachment from NATS object store.
pub async fn attachment_read(
    store: &ObjectStore,
    params: AttachmentReadParams,
) -> Result<CallToolResult> {
    // Parse the URL
    let url = CidUrl::parse(&params.url).context("parse cid: URL")?;

    // Reject plan URLs (not yet supported)
    if matches!(url, CidUrl::Plan { .. }) {
        return Ok(CallToolResult::error(vec![ContentBlock::text(
            "plan URLs not yet supported",
        )]));
    }

    // Only media URLs are supported for read
    let CidUrl::Media { .. } = &url else {
        return Ok(CallToolResult::error(vec![ContentBlock::text(
            "unknown URL type, only media and plan URLs are supported",
        )]));
    };

    let Some((bytes, mime_type)) = get_media(store, &url).await? else {
        return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "attachment not found: {}",
            params.url
        ))]));
    };

    // Handle based on MIME type
    if IMAGE_MIME_TYPES.iter().any(|t| mime_type == *t) {
        // Return as image block
        let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        Ok(CallToolResult::success(vec![ContentBlock::image(
            data, mime_type,
        )]))
    } else if is_text_mime(&mime_type) {
        text_attachment_result(bytes, params)
    } else {
        // Binary, non-displayable
        Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "cannot display binary content of type {} ({} bytes). image types are supported; other binary content cannot be displayed.",
            mime_type,
            format_size(bytes.len())
        ))]))
    }
}

fn text_attachment_result(bytes: Vec<u8>, params: AttachmentReadParams) -> Result<CallToolResult> {
    let byte_count = bytes.len();
    let text = String::from_utf8(bytes).context("decode text content as UTF-8")?;
    let default = TruncateOpts::default();
    let opts = TruncateOpts {
        offset: params.offset.or(default.offset),
        limit: params.limit.or(default.limit),
        head_lines: params.head_lines.unwrap_or(default.head_lines),
        tail_lines: params.tail_lines.unwrap_or(default.tail_lines),
        max_output_bytes: params.max_output_bytes.unwrap_or(default.max_output_bytes),
        ..default
    };
    let truncated = if let Some(pattern) = params.grep {
        filter_and_truncate(&text, &pattern, &opts)
    } else {
        truncate_output(&text, &opts)
    };
    let summary = format!("Read {} bytes from {}", format_size(byte_count), params.url);
    Ok(CallToolResult::success(vec![
        ContentBlock::text(truncated).with_audience(vec![Role::Assistant]),
        ContentBlock::text(summary).with_audience(vec![Role::User]),
    ]))
}

/// Create a new text attachment in NATS object store.
pub async fn attachment_create(
    store: &ObjectStore,
    caller_session: Option<&harnx_toolset::SessionRef>,
    params: AttachmentCreateParams,
) -> Result<CallToolResult> {
    if params.content.len() > MAX_ATTACHMENT_BYTES {
        return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "attachment payload too large: {} bytes exceeds the 5 MB limit",
            params.content.len()
        ))]));
    }

    // Validate caller session
    let caller = match caller_session {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "caller session identity required to create attachments",
            )]));
        }
    };

    // Validate MIME type is text
    if !is_text_mime(&params.mime_type) {
        return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "attachment_create only accepts text MIME types; got {}",
            params.mime_type
        ))]));
    }

    // Compute SHA256 hash
    let mut hasher = Sha256::new();
    hasher.update(params.content.as_bytes());
    let hash_bytes = hasher.finalize();
    let hash = hex_encode(&hash_bytes);

    // Convert to harnx_core::cid_url::SessionRef
    let core_session =
        harnx_core::cid_url::SessionRef::new(caller.agent.clone(), caller.session_id.clone())
            .context("invalid session reference")?;

    // Build the CID URL
    let url = media_cid_url(&core_session, &hash);

    // Store in object store
    put_media(store, &url, params.content.as_bytes(), &params.mime_type)
        .await
        .context("store attachment")?;

    // Return success with resource_link
    let url_str = url.to_string();
    let resource =
        rmcp::model::Resource::new(&url_str, "attachment").with_mime_type(&params.mime_type);
    Ok(CallToolResult::success(vec![
        ContentBlock::text(format!("Created attachment: {}", url_str)),
        ContentBlock::resource_link(resource),
    ]))
}

// --- Helpers ---

/// Check if a MIME type is text-like.
fn is_text_mime(mime: &str) -> bool {
    let lower = mime.to_lowercase();
    lower.starts_with("text/") || TEXT_MIME_TYPES.iter().any(|t| lower == *t)
}

/// Filter lines by regex pattern and truncate.
fn filter_and_truncate(text: &str, pattern: &str, opts: &TruncateOpts) -> String {
    use fancy_regex::Regex;
    let re = match Regex::new(pattern) {
        Ok(r) => r,
        Err(_) => {
            // Invalid regex, truncate directly
            return truncate_output(text, opts);
        }
    };

    let filtered: Vec<&str> = text
        .lines()
        .filter(|line| re.is_match(line).unwrap_or(false))
        .collect();
    let joined = filtered.join("\n");
    truncate_output(&joined, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_mime_detection() {
        assert!(is_text_mime("text/plain"));
        assert!(is_text_mime("text/markdown"));
        assert!(is_text_mime("application/json"));
        assert!(is_text_mime("APPLICATION/JSON"));
        assert!(!is_text_mime("image/png"));
        assert!(!is_text_mime("application/octet-stream"));
    }

    #[test]
    fn image_mime_detection() {
        assert!(IMAGE_MIME_TYPES.contains(&"image/png"));
        assert!(IMAGE_MIME_TYPES.contains(&"image/jpeg"));
        assert!(IMAGE_MIME_TYPES.contains(&"image/gif"));
        assert!(IMAGE_MIME_TYPES.contains(&"image/webp"));
    }
}
