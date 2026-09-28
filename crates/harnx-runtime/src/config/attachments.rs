//! Content-addressed filesystem access for image/binary attachments referenced
//! from session transcripts. For NATS sessions, the per-session directory is a
//! worker cache backed by the JetStream object store; local sessions use it as
//! their durable store. Runtime owns this filesystem layer and provider-specific
//! wire encoders; shared expansion/cache scaffolding lives in `harnx-core`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use harnx_core::attachments::{
    cid_for_data_url_with_session, expand_passthrough_reference, read_attachment,
    store_attachment_data_url, ExpandedAttachment, CID_PREFIX,
};
use harnx_core::cid_url::SessionRef;
use harnx_core::message::MessageContentPart;
use harnx_core::safety::{format_size, truncate_line};

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
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "bin",
    }
}

/// Persist one inline `data:` URI into session attachment store. Returns the
/// stable `cid:<sha256>` reference for transcript storage.
pub fn write_attachment(dir: &Path, data_url: &str) -> Result<String> {
    store_attachment_data_url(dir, data_url)
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

struct ExternalizedPart {
    index: usize,
    marker: String,
}

/// Format a canonical attachment marker for agent visibility.
///
/// Returns `[attachment: <cid> (<mime>, <formatted size>)]`.
/// - Normalizes MIME whitespace, truncates headers >128 chars.
/// - Size is human-readable via `format_size` (e.g., "3B", "1.5KB").
///
/// Used by both local externalization and NATS externalization paths.
pub(crate) fn attachment_marker(cid: &str, mime_type: &str, size: usize) -> String {
    let mime_type = mime_type.split_whitespace().collect::<Vec<_>>().join(" ");
    let mime_type = truncate_line(&mime_type, 128);
    format!("[attachment: {cid} ({mime_type}, {})]", format_size(size))
}

fn externalize_part(
    dir: &Path,
    session: &SessionRef,
    index: usize,
    part: &mut MessageContentPart,
    cid_to_filename: &mut HashMap<String, String>,
) -> Result<Option<ExternalizedPart>> {
    let MessageContentPart::ImageUrl { image_url } = part else {
        return Ok(None);
    };
    if !image_url.url.starts_with("data:") {
        return Ok(None);
    }

    let local_cid = write_attachment(dir, &image_url.url)?;
    let (bytes, mime_type) = read_attachment(dir, &local_cid)?;
    let cid = cid_for_data_url_with_session(session, &image_url.url);
    let ext = extension_for_data_url(&image_url.url);
    let hash = local_cid.trim_start_matches(CID_PREFIX);
    cid_to_filename.insert(cid.clone(), format!("{hash}.{ext}"));
    image_url.url = cid.clone();
    Ok(Some(ExternalizedPart {
        index,
        marker: attachment_marker(&cid, &mime_type, bytes.len()),
    }))
}

/// Replace inline data-URI media parts with persisted `cid:media:` references.
/// A text marker follows each rewritten part so models can pass its URL to tools.
/// Deduplicates by skipping insertion if an identical marker already exists at `index + 1`.
pub fn externalize_parts(
    dir: &Path,
    session: &SessionRef,
    parts: &mut Vec<MessageContentPart>,
    cid_to_filename: &mut HashMap<String, String>,
) -> Result<()> {
    let mut externalized = Vec::new();
    for (index, part) in parts.iter_mut().enumerate() {
        if let Some(item) = externalize_part(dir, session, index, part, cid_to_filename)? {
            externalized.push(item);
        }
    }
    for item in externalized.into_iter().rev() {
        let marker_exists = matches!(
            parts.get(item.index + 1),
            Some(MessageContentPart::Text { text }) if text == &item.marker
        );
        if !marker_exists {
            parts.insert(
                item.index + 1,
                MessageContentPart::Text { text: item.marker },
            );
        }
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
            extension_for_data_url("data:application/pdf;base64,AA"),
            "pdf"
        );
        assert_eq!(extension_for_data_url("data:text/plain;base64,AA"), "txt");
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
        assert_eq!(entries.len(), 2, "blob and MIME metadata are written");
        assert!(entries
            .iter()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".png")));
        assert!(entries
            .iter()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".mime")));

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
        assert_eq!(count, 2, "duplicate blob does not create extra files");
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

        let session = SessionRef::new(Some("pantheon/atlas".into()), "sess01".into()).unwrap();
        let mut map = HashMap::new();
        externalize_parts(&dir, &session, &mut parts, &mut map).unwrap();

        let cid = match &parts[1] {
            MessageContentPart::ImageUrl { image_url } => {
                assert!(image_url
                    .url
                    .starts_with("cid:media:pantheon%2Fatlas/sess01/"));
                assert!(!image_url.url.contains("QUJD"));
                image_url.url.clone()
            }
            other => panic!("expected ImageUrl, got {other:#?}"),
        };
        assert_eq!(
            parts[2],
            MessageContentPart::Text {
                text: format!("[attachment: {cid} (image/png, 3B)]")
            }
        );
        assert_eq!(map.len(), 1, "cid -> filename recorded");
        assert!(
            map.values().next().unwrap().ends_with(".png"),
            "recorded filename carries image extension"
        );
        externalize_parts(&dir, &session, &mut parts, &mut map).unwrap();
        assert_eq!(parts.len(), 3, "marker is not duplicated on another pass");

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
