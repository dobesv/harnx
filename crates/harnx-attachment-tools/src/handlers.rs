//! Attachment tool handlers.
//!
//! Implements `attachment_read` and `attachment_create` operations over NATS.

use anyhow::{Context, Result};
use async_nats::jetstream::object_store::ObjectStore;
use harnx_blob_store::{media_cid_url, put_media, resolve};
use harnx_core::cid_url::CidUrl;
use harnx_core::crypto::hex_encode;
use harnx_core::safety::{format_size, truncate_output, TruncateOpts};
use harnx_toolset_server::content::WithAudience;
use rmcp::model::{CallToolResult, ContentBlock, Role};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024; // 5 MB

/// Default byte limit when byte_offset is provided without byte_limit.
/// This prevents unbounded reads while still allowing reasonably large pages.
const DEFAULT_BYTE_LIMIT: usize = 64 * 1024; // 64 KiB

/// Image MIME types that return as image content blocks.
const IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Text-like MIME types that return as truncated text.
const TEXT_MIME_TYPES: &[&str] = &[
    "text/plain",
    "text/markdown",
    "text/html",
    "text/csv",
    "text/yaml",
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
    /// Maximum output bytes (applies after line selection).
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    /// Filter lines by regex pattern.
    #[serde(default)]
    pub grep: Option<String>,
    /// Zero-based byte offset to start reading.
    /// Cannot be used with line selectors (offset, limit, head_lines, tail_lines, grep).
    #[serde(default)]
    pub byte_offset: Option<usize>,
    /// Maximum number of bytes to return from byte_offset.
    /// Cannot be used with line selectors.
    /// Defaults to 64 KiB when byte_offset is provided without byte_limit.
    #[serde(default)]
    pub byte_limit: Option<usize>,
}

/// Check if any line-based selectors are used.
fn has_line_selectors(params: &AttachmentReadParams) -> bool {
    params.offset.is_some()
        || params.limit.is_some()
        || params.head_lines.is_some()
        || params.tail_lines.is_some()
        || params.grep.is_some()
}

/// Check if any byte-based selectors are used.
fn has_byte_selectors(params: &AttachmentReadParams) -> bool {
    params.byte_offset.is_some() || params.byte_limit.is_some()
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

/// Read an attachment or rendered plan from NATS-backed storage.
pub async fn attachment_read(
    jetstream: &async_nats::jetstream::Context,
    params: AttachmentReadParams,
) -> Result<CallToolResult> {
    // Check for conflicting selectors
    let has_line = has_line_selectors(&params);
    let has_byte = has_byte_selectors(&params);
    if has_line && has_byte {
        return Ok(CallToolResult::error(vec![ContentBlock::text(
            "cannot combine line selectors (offset, limit, head_lines, tail_lines, grep) with byte selectors (byte_offset, byte_limit)"
        )]));
    }

    let url = CidUrl::parse(&params.url).context("parse cid: URL")?;
    let resolved = match resolve(jetstream, &url).await {
        Ok(resolved) => resolved,
        Err(error) => {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                error.to_string(),
            )]));
        }
    };

    // Normalize MIME base type for dispatch while preserving original
    let normalized_mime = normalize_mime_base(&resolved.mime_type);

    if IMAGE_MIME_TYPES.iter().any(|mime| normalized_mime == *mime) {
        let data =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &resolved.bytes);
        return Ok(CallToolResult::success(vec![ContentBlock::image(
            data,
            resolved.mime_type,
        )]));
    }
    if is_text_mime(&normalized_mime) {
        // Dispatch to byte paging if byte selectors present, else line-based
        if has_byte {
            return byte_text_attachment_result(resolved.bytes, params);
        }
        return text_attachment_result(resolved.bytes, params);
    }
    Ok(binary_attachment_error(
        &resolved.mime_type,
        resolved.bytes.len(),
    ))
}

/// Normalize MIME type by extracting base type (before any parameters like charset).
fn normalize_mime_base(mime: &str) -> String {
    mime.split(';')
        .next()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| mime.to_lowercase())
}

fn binary_attachment_error(mime_type: &str, byte_count: usize) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "cannot display binary content of type {} ({} bytes). image types are supported; other binary content cannot be displayed.",
        mime_type,
        format_size(byte_count)
    ))])
}

/// Byte offsets and metadata for a single contiguous text page.
struct BytePage {
    start: usize,
    end: usize,
    total: usize,
}

impl BytePage {
    fn new(bytes: &[u8], params: &AttachmentReadParams) -> Result<Self, String> {
        let total = bytes.len();
        let start = params.byte_offset.unwrap_or(0).min(total);
        if !is_char_start(bytes, start) {
            return Err(format!(
                "byte_offset {} is not at a valid UTF-8 character boundary; next valid offset is {}",
                start, find_next_char_boundary(bytes, start)
            ));
        }
        let limit = params.byte_limit.unwrap_or(DEFAULT_BYTE_LIMIT);
        if limit == 0 {
            return Err("byte_limit must be greater than zero".to_string());
        }
        let proposed_end = start.saturating_add(limit).min(total);
        let mut end = find_prev_char_boundary(bytes, proposed_end);
        // A tiny limit must still return one complete character, never a stalled page.
        if end <= start && start < total {
            end = find_next_char_boundary(bytes, start + 1);
        }
        Ok(Self { start, end, total })
    }

    fn result(&self, bytes: &[u8], url: &str) -> Result<CallToolResult, String> {
        // Boundary scans don't validate stored bytes. Both normal and tiny pages
        // must decode here so malformed blobs return tool errors, not panics.
        let text = std::str::from_utf8(&bytes[self.start..self.end]).map_err(|error| {
            format!(
                "invalid UTF-8 sequence at byte boundaries {}..{}: {}",
                self.start, self.end, error
            )
        })?;
        Ok(CallToolResult::success(vec![
            ContentBlock::text(text).with_audience(vec![Role::Assistant]),
            ContentBlock::text(self.summary(url)).with_audience(vec![Role::User]),
        ]))
    }

    fn summary(&self, url: &str) -> String {
        let continuation = if self.end < self.total {
            format!("next_byte_offset: {}", self.end)
        } else {
            "end_of_content: true".to_string()
        };
        format!(
            "Read {} bytes (offset {} to {}/{}) from {} | {}",
            self.end - self.start,
            self.start,
            self.end,
            self.total,
            url,
            continuation
        )
    }
}

fn byte_text_attachment_result(
    bytes: Vec<u8>,
    params: AttachmentReadParams,
) -> Result<CallToolResult> {
    let result = BytePage::new(&bytes, &params).and_then(|page| page.result(&bytes, &params.url));
    Ok(result.unwrap_or_else(|error| CallToolResult::error(vec![ContentBlock::text(error)])))
}

fn find_next_char_boundary(bytes: &[u8], pos: usize) -> usize {
    let mut boundary = pos.min(bytes.len());
    while !is_char_start(bytes, boundary) {
        boundary += 1;
    }
    boundary
}

fn find_prev_char_boundary(bytes: &[u8], pos: usize) -> usize {
    let mut boundary = pos.min(bytes.len());
    while boundary > 0 && !is_char_start(bytes, boundary) {
        boundary -= 1;
    }
    boundary
}

fn is_char_start(bytes: &[u8], pos: usize) -> bool {
    bytes
        .get(pos)
        .is_none_or(|byte| !(0x80..0xC0).contains(byte))
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

/// Check if a MIME type is text-like (suitable for text/block output).
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
    use serde_json::json;

    #[test]
    fn text_mime_detection() {
        for (mime, expected) in [
            ("text/plain", true),
            ("text/markdown", true),
            ("application/json", true),
            ("APPLICATION/JSON", true),
            ("application/yaml", true),
            ("application/x-yaml", true),
            ("image/png", false),
            ("application/octet-stream", false),
        ] {
            assert_eq!(is_text_mime(mime), expected, "{mime}");
        }
    }

    #[test]
    fn image_mime_detection() {
        for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert!(IMAGE_MIME_TYPES.contains(&mime), "{mime}");
        }
    }

    #[test]
    fn normalize_mime_base_strips_params() {
        for (mime, expected) in [
            ("text/plain; charset=utf-8", "text/plain"),
            ("application/json; charset=utf-8", "application/json"),
            ("text/plain", "text/plain"),
            ("TEXT/PLAIN", "text/plain"),
            (" Application/X-Yaml ; charset=utf-8", "application/x-yaml"),
            ("application/yaml; charset=utf-8", "application/yaml"),
        ] {
            assert_eq!(normalize_mime_base(mime), expected);
            assert!(is_text_mime(&normalize_mime_base(mime)), "{mime}");
        }
    }

    fn assert_boundaries(bytes: &[u8], cases: &[(usize, usize)], forward: bool) {
        for &(position, expected) in cases {
            let actual = if forward {
                find_next_char_boundary(bytes, position)
            } else {
                find_prev_char_boundary(bytes, position)
            };
            assert_eq!(actual, expected, "boundary at {position}");
        }
    }

    #[test]
    fn find_utf8_char_end_handles_ascii() {
        assert_boundaries(b"hello world", &[(5, 5), (0, 0), (11, 11)], false);
    }

    #[test]
    fn find_utf8_char_end_handles_multibyte() {
        assert_boundaries(
            "héllo".as_bytes(),
            &[(0, 0), (1, 1), (2, 1), (3, 3), (100, 6)],
            false,
        );
    }

    #[test]
    fn find_utf8_char_end_handles_truncated_multibyte() {
        assert_boundaries(
            "日本語".as_bytes(),
            &[(0, 0), (3, 3), (4, 3), (6, 6), (7, 6), (9, 9)],
            false,
        );
    }

    #[test]
    fn find_next_char_boundary_finds_forward() {
        assert_boundaries(
            "日本語".as_bytes(),
            &[(1, 3), (2, 3), (4, 6), (5, 6), (9, 9)],
            true,
        );
    }

    #[test]
    fn find_prev_char_boundary_backs_up() {
        assert_boundaries(
            "日本語".as_bytes(),
            &[(1, 0), (2, 0), (3, 3), (4, 3), (5, 3)],
            false,
        );
    }

    #[test]
    fn find_char_end_from_start_returns_char_width() {
        assert_boundaries("日本語".as_bytes(), &[(1, 3), (4, 6), (7, 9)], true);
        assert_boundaries(b"hello", &[(1, 1), (2, 2)], true);
    }

    #[test]
    fn has_line_selectors_detects_line_params() {
        assert_selectors(
            &["offset", "limit", "head_lines", "tail_lines", "grep"],
            true,
        );
    }

    #[test]
    fn has_byte_selectors_detects_byte_params() {
        assert_selectors(&["byte_offset", "byte_limit"], false);
    }

    fn assert_selectors(selectors: &[&str], line_mode: bool) {
        let empty = json!({ "url": "test" });
        let params = serde_json::from_value(empty.clone()).unwrap();
        assert!(!has_line_selectors(&params) && !has_byte_selectors(&params));
        for selector in selectors {
            let mut value = empty.clone();
            value[selector] = if *selector == "grep" {
                json!("test")
            } else {
                json!(1)
            };
            let params = serde_json::from_value(value).unwrap();
            assert_eq!(has_line_selectors(&params), line_mode, "{selector}");
            assert_eq!(has_byte_selectors(&params), !line_mode, "{selector}");
        }
    }

    #[test]
    fn byte_paging_rejects_invalid_offset() {
        let params = serde_json::from_value(json!({"url": "test", "byte_offset": 1})).unwrap();
        let error = BytePage::new("日本語".as_bytes(), &params).err().unwrap();
        assert!(error.contains("next valid offset is 3"));
    }

    #[test]
    fn byte_paging_handles_page_boundary_correctly() {
        let bytes = "日本語".as_bytes();
        let params = serde_json::from_value(json!({"url": "test", "byte_limit": 4})).unwrap();
        let page = BytePage::new(bytes, &params).unwrap();
        assert_eq!(bytes.len(), 9);
        assert_eq!(page.end, 3);
        assert_eq!(
            std::str::from_utf8(&bytes[page.start..page.end]).unwrap(),
            "日"
        );
    }

    #[test]
    fn is_char_start_detects_boundaries() {
        for position in 0..=9 {
            assert_eq!(
                is_char_start("日本語".as_bytes(), position),
                position % 3 == 0
            );
        }
    }

    #[test]
    fn byte_pages_reject_malformed_sequences_without_panicking() {
        for bytes in [
            vec![0xE0, 0x80, 0x00],
            vec![0xF0, 0x9F],
            vec![0xFF],
            vec![0xC0, 0x80],
        ] {
            for limit in [1, 2, 4] {
                let params =
                    serde_json::from_value(json!({"url": "test", "byte_limit": limit})).unwrap();
                let result = byte_text_attachment_result(bytes.clone(), params).unwrap();
                assert_eq!(
                    result.is_error,
                    Some(true),
                    "bytes {bytes:?}, limit {limit}"
                );
            }
        }
    }
}
