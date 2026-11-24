use harnx_core::crypto::base64_encode;
use harnx_toolset_server::content::WithAudience;
use reqwest::header::HeaderValue;
use rmcp::model::{CallToolResult, ContentBlock, Role};

use crate::client::FetchedPage;

const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

pub(super) fn try_image_result(page: &FetchedPage) -> Result<Option<CallToolResult>, String> {
    let Some(mime) = classify_image(page)? else {
        return Ok(None);
    };
    let url = &page.final_url;
    let size = page.body.len();
    if size > MAX_IMAGE_BYTES {
        return Err(format!(
            "image at '{url}' is too large ({size} bytes; limit is 5 MiB / {MAX_IMAGE_BYTES} bytes)"
        ));
    }

    let data = base64_encode(&page.body);
    let summary = format!("Fetched image '{url}' ({mime}, {size} bytes)");
    Ok(Some(CallToolResult::success(vec![
        ContentBlock::image(data, mime.to_owned()).with_audience(vec![Role::Assistant]),
        ContentBlock::text(summary).with_audience(vec![Role::User]),
    ])))
}

fn classify_image(page: &FetchedPage) -> Result<Option<&'static str>, String> {
    let Some(declared) = page.content_type.as_ref().map(base_media_type) else {
        return Ok(image_mime_from_magic(&page.body));
    };

    if declared.eq_ignore_ascii_case("application/octet-stream") {
        return Ok(image_mime_from_magic(&page.body));
    }
    if !declared
        .split_once('/')
        .is_some_and(|(kind, _)| kind.eq_ignore_ascii_case("image"))
    {
        return Ok(None);
    }

    image_mime_from_magic(&page.body).map(Some).ok_or_else(|| {
        format!(
            "unsupported image format at '{}' (Content-Type: {declared}); supported formats: PNG, JPEG, GIF, WebP",
            page.final_url
        )
    })
}

fn base_media_type(value: &HeaderValue) -> &str {
    value
        .as_bytes()
        .split(|&byte| byte == b';')
        .next()
        .and_then(|base| std::str::from_utf8(base).ok())
        .unwrap_or_default()
        .trim()
}

// Match fs_read's magic-byte checks without its filename-extension fallback.
fn image_mime_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    const WEBP_HEADER_LEN: usize = 12;
    if bytes.len() >= WEBP_HEADER_LEN && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}
