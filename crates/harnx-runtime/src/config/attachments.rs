//! Content-addressed storage for image/binary attachments referenced from
//! NATS session transcripts. Blobs live in a per-session attachment directory
//! and are referenced from message content as `cid:<sha256>`, keeping
//! multi-megabyte base64 out of NATS and out of memory at rest. Runtime owns
//! session-local blob storage and provider-specific wire
//! encoders; shared expansion/cache scaffolding lives in `harnx-core`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use harnx_core::attachments::{
    cid_for_data_url, expand_passthrough_reference, read_attachment, ExpandedAttachment, CID_PREFIX,
};
use harnx_core::crypto::base64_decode;
use harnx_core::message::MessageContentPart;

/// Map a data URI's MIME type to a file extension. Defaults to `bin` for
/// unrecognised types.
pub fn extension_for_data_url(data_url: &str) -> &'static str {
    let mime = data_url
        .strip_prefix("data:")
        .and_then(|rest| rest.split(';').next())
        .unwrap_or("");
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

/// Parse an inline data URI into (`mime_type`, `raw_bytes`). Only `;base64,`
/// form is supported because that's what upstream message schemas already use
/// for image parts.
fn parse_data_url(data_url: &str) -> Result<(&str, Vec<u8>)> {
    let rest = data_url
        .strip_prefix("data:")
        .ok_or_else(|| anyhow::anyhow!("attachment must be a data: URI"))?;
    let (mime, b64) = rest
        .split_once(";base64,")
        .ok_or_else(|| anyhow::anyhow!("attachment data URI must contain ;base64,"))?;
    let bytes = base64_decode(b64).context("invalid base64 in data URI")?;
    Ok((mime, bytes))
}

/// Persist one inline `data:` URI into session attachment store. Returns the
/// stable `cid:<sha256>` reference for transcript storage.
pub fn write_attachment(dir: &Path, data_url: &str) -> Result<String> {
    let cid = cid_for_data_url(data_url);
    let (_, bytes) = parse_data_url(data_url)?;
    let ext = extension_for_data_url(data_url);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create attachments dir {}", dir.display()))?;

    let hash = cid.trim_start_matches(CID_PREFIX);
    let path = dir.join(format!("{hash}.{ext}"));
    if !path.exists() {
        std::fs::write(&path, bytes)
            .with_context(|| format!("Failed to write attachment {}", path.display()))?;
    }
    Ok(cid)
}

#[allow(dead_code)]
pub trait AttachmentEncoder {
    /// Expand a transcript-stored image reference into provider wire form.
    fn expand(&self, dir: &Path, reference: &str) -> Result<ExpandedAttachment>;
}

pub struct Base64Encoder;

impl AttachmentEncoder for Base64Encoder {
    fn expand(&self, dir: &Path, reference: &str) -> Result<ExpandedAttachment> {
        if !reference.starts_with(CID_PREFIX) {
            return Ok(expand_passthrough_reference(reference));
        }

        let (bytes, mime_type) = read_attachment(dir, reference)?;
        Ok(ExpandedAttachment::DataUri {
            data: harnx_core::crypto::base64_encode(&bytes),
            mime_type,
        })
    }
}

/// Replace inline data-URI image parts with persisted `cid:` references and
/// record the original filename mapping for UI/export. Non-image parts and
/// already-externalized refs are left untouched.
pub fn externalize_parts(
    dir: &Path,
    parts: &mut [MessageContentPart],
    cid_to_filename: &mut HashMap<String, String>,
) -> Result<()> {
    for part in parts.iter_mut() {
        let MessageContentPart::ImageUrl { image_url } = part else {
            continue;
        };
        if !image_url.url.starts_with("data:") {
            continue;
        }
        let cid = write_attachment(dir, &image_url.url)?;
        let ext = extension_for_data_url(&image_url.url);
        cid_to_filename.insert(
            cid.clone(),
            format!("{}.{}", cid.trim_start_matches(CID_PREFIX), ext),
        );
        image_url.url = cid;
    }
    Ok(())
}

/// Expand persisted `cid:` image refs into provider wire format using
/// attachment encoder. Missing blobs degrade to text placeholder instead of
/// failing whole transcript load.
#[allow(dead_code)]
pub fn expand_parts(
    encoder: &dyn AttachmentEncoder,
    dir: &Path,
    parts: &mut [MessageContentPart],
) -> Result<()> {
    for part in parts.iter_mut() {
        let MessageContentPart::ImageUrl { image_url } = part else {
            continue;
        };
        if !image_url.url.starts_with(CID_PREFIX) {
            continue;
        }
        match encoder.expand(dir, &image_url.url) {
            Ok(ExpandedAttachment::DataUri { data, mime_type }) => {
                image_url.url = format!("data:{mime_type};base64,{data}");
            }
            Ok(ExpandedAttachment::RemoteRef { ref_id, .. }) => {
                image_url.url = ref_id;
            }
            Err(err) => {
                *part = MessageContentPart::Text {
                    text: format!(
                        "[attachment unavailable: {}]",
                        err.to_string().replace('\n', " ")
                    ),
                };
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::ImageUrl;

    #[test]
    fn extension_is_derived_from_mime() {
        assert_eq!(extension_for_data_url("data:image/png;base64,AA"), "png");
        assert_eq!(extension_for_data_url("data:image/jpeg;base64,AA"), "jpg");
        assert_eq!(
            extension_for_data_url("data:application/x;base64,AA"),
            "bin"
        );
    }

    #[test]
    fn write_then_base64_expand_round_trips() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("s.attachments");
        let data_url = "data:image/png;base64,QUJD".to_string();

        let cid = write_attachment(&dir, &data_url).unwrap();
        assert!(cid.starts_with(CID_PREFIX));
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "exactly one attachment file written");
        assert!(entries[0].file_name().to_string_lossy().ends_with(".png"));

        let encoder = Base64Encoder;
        let restored = encoder.expand(&dir, &cid).unwrap();
        assert_eq!(
            restored,
            ExpandedAttachment::DataUri {
                data: "QUJD".into(),
                mime_type: "image/png".into(),
            },
            "expand reproduces data URI payload and MIME"
        );

        let cid2 = write_attachment(&dir, &data_url).unwrap();
        assert_eq!(cid, cid2);
        let count = std::fs::read_dir(&dir).unwrap().flatten().count();
        assert_eq!(count, 1, "duplicate blob does not create a second file");
    }

    #[test]
    fn expand_non_cid_data_url_splits_fields() {
        let encoder = Base64Encoder;
        let expanded = encoder
            .expand(Path::new("."), "data:image/png;base64,QUJD")
            .unwrap();

        assert_eq!(
            expanded,
            ExpandedAttachment::DataUri {
                data: "QUJD".into(),
                mime_type: "image/png".into(),
            }
        );
    }

    #[test]
    fn expand_non_cid_remote_url_keeps_reference() {
        let encoder = Base64Encoder;
        let expanded = encoder
            .expand(Path::new("."), "https://example.com/image.png")
            .unwrap();

        assert_eq!(
            expanded,
            ExpandedAttachment::RemoteRef {
                ref_id: "https://example.com/image.png".into(),
                mime_type: String::new(),
                expires_at: None,
            }
        );
    }

    #[test]
    fn expand_parts_drops_unresolvable_cid() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("missing.attachments");
        let mut parts = vec![MessageContentPart::ImageUrl {
            image_url: ImageUrl {
                url: "cid:deadbeef".into(),
            },
        }];
        let encoder = Base64Encoder;
        expand_parts(&encoder, &dir, &mut parts).unwrap();
        match &parts[0] {
            MessageContentPart::Text { text } => assert!(text.contains("unavailable")),
            other => panic!("expected Text placeholder, got {other:#?}"),
        }
    }

    #[test]
    fn externalize_then_expand_parts_round_trips() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("s.attachments");
        let data_url = "data:image/png;base64,QUJD".to_string();
        let mut parts = vec![
            MessageContentPart::Text { text: "hi".into() },
            MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: data_url.clone(),
                },
            },
        ];

        let mut map = HashMap::new();
        externalize_parts(&dir, &mut parts, &mut map).unwrap();

        match &parts[1] {
            MessageContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with(CID_PREFIX));
                assert!(!image_url.url.contains("QUJD"));
            }
            other => panic!("expected ImageUrl, got {other:#?}"),
        }
        assert_eq!(map.len(), 1, "cid -> filename recorded");
        assert!(
            map.values().next().unwrap().ends_with(".png"),
            "recorded filename carries image extension"
        );

        let encoder = Base64Encoder;
        expand_parts(&encoder, &dir, &mut parts).unwrap();
        match &parts[1] {
            MessageContentPart::ImageUrl { image_url } => assert_eq!(image_url.url, data_url),
            other => panic!("expected ImageUrl, got {other:#?}"),
        }
        match &parts[0] {
            MessageContentPart::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected Text, got {other:#?}"),
        }
    }
}
