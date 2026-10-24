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

/// Handle byte-paged text attachment result with bounded UTF-8 safe slices.
fn byte_text_attachment_result(
    bytes: Vec<u8>,
    params: AttachmentReadParams,
) -> Result<CallToolResult> {
    let total_bytes = bytes.len();

    // Determine start position with boundary validation
    let requested_start = params.byte_offset.unwrap_or(0).min(total_bytes);

    // Validate that requested_start is at a character boundary
    if requested_start > 0 && !is_char_start(&bytes, requested_start) {
        // Find the next valid character boundary
        let next_valid = find_next_char_boundary(&bytes, requested_start);
        return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "byte_offset {} is not at a valid UTF-8 character boundary; next valid offset is {}",
            requested_start, next_valid
        ))]));
    }

    // Use DEFAULT_BYTE_LIMIT when byte_offset is provided without byte_limit
    let limit = params.byte_limit.unwrap_or(DEFAULT_BYTE_LIMIT);

    // Calculate the proposed end position
    let proposed_end = (requested_start + limit).min(total_bytes);

    // Find the actual safe end that doesn't split a multibyte character
    // We want to include as many complete characters as fit within the limit
    let safe_end = if proposed_end == total_bytes {
        // At end of content, it's always safe
        total_bytes
    } else if is_char_start(&bytes, proposed_end) {
        // Proposed end is right at a character boundary
        proposed_end
    } else {
        // Proposed end is inside a multibyte char, back up to previous boundary
        find_prev_char_boundary(&bytes, proposed_end)
    };

    // Make sure we make forward progress unless at end
    if safe_end <= requested_start && requested_start < total_bytes {
        // Could not fit even one character within the limit
        // Find the end of the current character so we can return it
        let char_end = find_char_end_from_start(&bytes, requested_start);
        if char_end <= requested_start {
            // This shouldn't happen with valid UTF-8
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "internal error: cannot find character boundary from offset {}",
                requested_start
            ))]));
        }
        // Return full character even though it exceeds requested limit
        let page_bytes = &bytes[requested_start..char_end];
        let page_text = std::str::from_utf8(page_bytes)
            .expect("slice from valid boundaries")
            .to_string();

        let mut content = vec![ContentBlock::text(page_text).with_audience(vec![Role::Assistant])];
        content.push(
            ContentBlock::text(format!(
                "Read {} bytes (offset {} to {}/{}) from {} | {}",
                char_end - requested_start,
                requested_start,
                char_end,
                total_bytes,
                params.url,
                if char_end < total_bytes {
                    format!("next_byte_offset: {}", char_end)
                } else {
                    "end_of_content: true".to_string()
                }
            ))
            .with_audience(vec![Role::User]),
        );
        return Ok(CallToolResult::success(content));
    }

    let page_bytes = &bytes[requested_start..safe_end];
    let page_text = match std::str::from_utf8(page_bytes) {
        Ok(s) => s.to_string(),
        Err(e) => {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "internal error: invalid UTF-8 sequence at byte boundaries {}..{}: {}",
                requested_start, safe_end, e
            ))]));
        }
    };

    // Calculate next offset for pagination
    let next_byte_offset = if safe_end < total_bytes {
        Some(safe_end)
    } else {
        None
    };

    let mut content = vec![ContentBlock::text(page_text).with_audience(vec![Role::Assistant])];

    // Include pagination metadata in user-audience block
    let mut meta_parts = vec![format!(
        "Read {} bytes (offset {} to {}/{}) from {}",
        safe_end - requested_start,
        requested_start,
        safe_end,
        total_bytes,
        params.url
    )];
    if let Some(next) = next_byte_offset {
        meta_parts.push(format!("next_byte_offset: {}", next));
    } else {
        meta_parts.push("end_of_content: true".to_string());
    }
    content.push(ContentBlock::text(meta_parts.join(" | ")).with_audience(vec![Role::User]));

    Ok(CallToolResult::success(content))
}

/// Find the next character boundary at or after `pos`.
/// Used for error messages to tell caller where to start.
fn find_next_char_boundary(bytes: &[u8], pos: usize) -> usize {
    let mut boundary = pos.min(bytes.len());
    while boundary < bytes.len() && !is_char_start(bytes, boundary) {
        boundary += 1;
    }
    boundary
}

/// Find the previous character boundary before `pos`.
/// Used to back up from inside a multibyte char.
fn find_prev_char_boundary(bytes: &[u8], pos: usize) -> usize {
    let mut boundary = pos.min(bytes.len());
    while boundary > 0 && !is_char_start(bytes, boundary) {
        boundary -= 1;
    }
    boundary
}

/// Find the end of the character starting at `start`, which must be a valid char start.
fn find_char_end_from_start(bytes: &[u8], start: usize) -> usize {
    if start >= bytes.len() {
        return bytes.len();
    }
    // The character length is determined by its first byte
    let first_byte = bytes[start];
    let char_len = if first_byte < 0x80 {
        1 // ASCII
    } else if (0xC0..0xE0).contains(&first_byte) {
        2
    } else if (0xE0..0xF0).contains(&first_byte) {
        3
    } else if (0xF0..0xF8).contains(&first_byte) {
        4
    } else {
        1 // Invalid/continuation (shouldn't happen)
    };
    (start + char_len).min(bytes.len())
}

/// Find the nearest valid UTF-8 character boundary at or before `max_pos`.
/// This ensures bytes[0..result] is valid UTF-8 when starting from 0.
/// Returns the boundary where a valid slice would end.
fn find_utf8_char_end(bytes: &[u8], max_pos: usize) -> usize {
    let pos = max_pos.min(bytes.len());

    if pos == 0 {
        return 0;
    }

    // If we're at a character boundary or at end, this is a valid position
    if pos == bytes.len() || is_char_start(bytes, pos) {
        return pos;
    }

    // We're inside a multibyte character - walk backward to find the boundary
    let mut boundary = pos;
    for _ in 0..4 {
        if boundary == 0 {
            return 0;
        }
        if is_char_start(bytes, boundary) {
            return boundary;
        }
        boundary -= 1;
    }

    boundary
}

/// Check if a byte is a UTF-8 continuation byte (0x80..0xBF).
#[allow(dead_code, clippy::manual_range_contains)]
fn is_continuation_byte(byte: u8) -> bool {
    byte >= 0x80 && byte < 0xC0
}

/// Check if position is at the start of a UTF-8 character.
#[allow(clippy::manual_range_contains)]
fn is_char_start(bytes: &[u8], pos: usize) -> bool {
    if pos >= bytes.len() {
        return true; // End position
    }

    let byte = bytes[pos];

    // ASCII byte or start of multibyte sequence
    byte < 0x80 || byte >= 0xC0
}

/// Find the nearest valid UTF-8 character boundary at or before the given position.
/// Alias for find_utf8_char_end for backwards compatibility.
#[allow(dead_code)]
fn find_utf8_boundary(bytes: &[u8], max_pos: usize) -> usize {
    find_utf8_char_end(bytes, max_pos)
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

    #[test]
    fn normalize_mime_base_strips_params() {
        assert_eq!(
            normalize_mime_base("text/plain; charset=utf-8"),
            "text/plain"
        );
        assert_eq!(
            normalize_mime_base("application/json; charset=utf-8"),
            "application/json"
        );
        assert_eq!(normalize_mime_base("text/plain"), "text/plain");
        assert_eq!(normalize_mime_base("TEXT/PLAIN"), "text/plain");
    }

    #[test]
    fn find_utf8_char_end_handles_ascii() {
        let bytes = b"hello world";
        assert_eq!(find_utf8_char_end(bytes, 5), 5);
        assert_eq!(find_utf8_char_end(bytes, 0), 0);
        assert_eq!(find_utf8_char_end(bytes, 11), 11);
    }

    #[test]
    fn find_utf8_char_end_handles_multibyte() {
        // "héllo" - bytes: [h, 0xC3, 0xA9, l, l, o] where é is bytes 1-2
        let bytes = "héllo".as_bytes();

        // Position 0 is ASCII character boundary (h)
        assert_eq!(find_utf8_char_end(bytes, 0), 0);

        // Position 1 is start of multi-byte sequence (0xC3), valid boundary
        assert_eq!(find_utf8_char_end(bytes, 1), 1);

        // Position 2 is 0xA9, the second byte of é (continuation byte) - should back up to 1
        assert_eq!(find_utf8_char_end(bytes, 2), 1);

        // Position 3 is ASCII (l), valid boundary
        assert_eq!(find_utf8_char_end(bytes, 3), 3);

        // Positions past end return end of string
        assert_eq!(find_utf8_char_end(bytes, 100), bytes.len());
    }

    #[test]
    fn find_utf8_char_end_handles_truncated_multibyte() {
        // "日本語" - each character is 3 bytes in UTF-8
        // bytes: [0xE6, 0x97, 0xA5, 0xE6, 0x9C, 0xAC, 0xE8, 0xAA, 0x9E]
        let bytes = "日本語".as_bytes();

        // Position 0: start of 日 (0xE6), valid boundary
        assert_eq!(find_utf8_char_end(bytes, 0), 0);

        // Position 3: start of 本 (0xE6), valid boundary
        assert_eq!(find_utf8_char_end(bytes, 3), 3);

        // Position 4: inside 本 (continuation byte), should back up to 3
        assert_eq!(find_utf8_char_end(bytes, 4), 3);

        // Position 6: start of 語 (0xE8), valid boundary
        assert_eq!(find_utf8_char_end(bytes, 6), 6);

        // Position 7: inside 語, should back up to 6
        assert_eq!(find_utf8_char_end(bytes, 7), 6);

        // Position 9: end of string
        assert_eq!(find_utf8_char_end(bytes, 9), 9);
    }

    #[test]
    fn find_next_char_boundary_finds_forward() {
        let bytes = "日本語".as_bytes(); // 3 chars x 3 bytes = 9 bytes

        // Position 1-2 are inside 日, should find 3 (start of 本)
        assert_eq!(find_next_char_boundary(bytes, 1), 3);
        assert_eq!(find_next_char_boundary(bytes, 2), 3);

        // Position 4-5 are inside 本, should find 6 (start of 語)
        assert_eq!(find_next_char_boundary(bytes, 4), 6);
        assert_eq!(find_next_char_boundary(bytes, 5), 6);

        // Position 9+ returns end
        assert_eq!(find_next_char_boundary(bytes, 9), 9);
    }

    #[test]
    fn find_prev_char_boundary_backs_up() {
        let bytes = "日本語".as_bytes(); // 9 bytes

        // Position 1-2 are inside 日, should back up to 0
        assert_eq!(find_prev_char_boundary(bytes, 1), 0);
        assert_eq!(find_prev_char_boundary(bytes, 2), 0);

        // Position 3 is boundary
        assert_eq!(find_prev_char_boundary(bytes, 3), 3);

        // Position 4-5 are inside 本, should back up to 3
        assert_eq!(find_prev_char_boundary(bytes, 4), 3);
        assert_eq!(find_prev_char_boundary(bytes, 5), 3);
    }

    #[test]
    fn find_char_end_from_start_returns_char_width() {
        let bytes = "日本語".as_bytes();

        // 日本語 each char is 3 bytes
        assert_eq!(find_char_end_from_start(bytes, 0), 3);
        assert_eq!(find_char_end_from_start(bytes, 3), 6);
        assert_eq!(find_char_end_from_start(bytes, 6), 9);

        // ASCII
        let ascii = b"hello";
        assert_eq!(find_char_end_from_start(ascii, 0), 1);
        assert_eq!(find_char_end_from_start(ascii, 1), 2);
    }

    #[test]
    fn has_line_selectors_detects_line_params() {
        let params_no_line = AttachmentReadParams {
            url: "test".to_string(),
            offset: None,
            limit: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
            byte_offset: None,
            byte_limit: None,
        };
        assert!(!has_line_selectors(&params_no_line));

        let params_with_offset = AttachmentReadParams {
            url: "test".to_string(),
            offset: Some(1),
            limit: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
            byte_offset: None,
            byte_limit: None,
        };
        assert!(has_line_selectors(&params_with_offset));

        let params_with_grep = AttachmentReadParams {
            url: "test".to_string(),
            offset: None,
            limit: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: Some("test".to_string()),
            byte_offset: None,
            byte_limit: None,
        };
        assert!(has_line_selectors(&params_with_grep));
    }

    #[test]
    fn has_byte_selectors_detects_byte_params() {
        let params_no_byte = AttachmentReadParams {
            url: "test".to_string(),
            offset: None,
            limit: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
            byte_offset: None,
            byte_limit: None,
        };
        assert!(!has_byte_selectors(&params_no_byte));

        let params_with_offset = AttachmentReadParams {
            url: "test".to_string(),
            offset: None,
            limit: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
            byte_offset: Some(100),
            byte_limit: None,
        };
        assert!(has_byte_selectors(&params_with_offset));

        let params_with_limit = AttachmentReadParams {
            url: "test".to_string(),
            offset: None,
            limit: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
            byte_offset: None,
            byte_limit: Some(1000),
        };
        assert!(has_byte_selectors(&params_with_limit));
    }

    #[test]
    fn byte_paging_rejects_invalid_offset() {
        // Test that offset inside multibyte char returns error
        let bytes = "日本語".as_bytes();
        let next = find_next_char_boundary(bytes, 1);
        // offset 1 (inside 日) should find 3 (start of 本)
        assert_eq!(next, 3);
    }

    #[test]
    fn byte_paging_handles_page_boundary_correctly() {
        // Test that splitting at multibyte boundary works
        let bytes = "日本語".as_bytes(); // 9 bytes total, 3 chars
        assert_eq!(bytes.len(), 9);

        // Page 1: limit 4 bytes, should return only first char (3 bytes)
        let safe_end = find_prev_char_boundary(bytes, 4);
        assert_eq!(safe_end, 3);

        // Verify the slice is valid UTF-8
        let slice = &bytes[0..safe_end];
        assert!(std::str::from_utf8(slice).is_ok());
        assert_eq!(std::str::from_utf8(slice).unwrap(), "日");
    }

    #[test]
    fn is_char_start_detects_boundaries() {
        let bytes = "日本語".as_bytes();

        // Character boundaries at 0, 3, 6
        assert!(is_char_start(bytes, 0));
        assert!(is_char_start(bytes, 3));
        assert!(is_char_start(bytes, 6));

        // Inside characters at 1, 2, 4, 5, 7, 8
        assert!(!is_char_start(bytes, 1));
        assert!(!is_char_start(bytes, 2));
        assert!(!is_char_start(bytes, 4));
        assert!(!is_char_start(bytes, 5));
        assert!(!is_char_start(bytes, 7));
        assert!(!is_char_start(bytes, 8));
    }
}
