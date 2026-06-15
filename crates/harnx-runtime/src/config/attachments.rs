//! Content-addressed storage for image/binary attachments referenced from
//! session transcripts. Blobs live in a per-session `{id}.attachments/`
//! directory and are referenced from message content as `cid:<sha256>`,
//! keeping multi-megabyte base64 out of the transcript and out of memory at
//! rest. The wire encoding is pluggable (`AttachmentEncoder`); only the
//! base64 backend ships today, with provider upload-API backends to follow.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use harnx_core::crypto::{base64_decode, base64_encode, sha256};
use harnx_core::message::MessageContentPart;

/// Prefix marking a content reference in `ImageUrl.url`.
pub const CID_PREFIX: &str = "cid:";

/// Compute the content id for an inline `data:` URI. The id is the SHA-256
/// of the full data URI string (consistent with the existing `data_urls`
/// keying), so identical blobs collapse to one id.
pub fn cid_for_data_url(data_url: &str) -> String {
    format!("{CID_PREFIX}{}", sha256(data_url))
}

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

/// The attachments directory for a session given its `.yaml` transcript path:
/// `<dir>/<stem>.attachments/`.
pub fn attachments_dir_for(session_yaml_path: &Path) -> PathBuf {
    let stem = session_yaml_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent = session_yaml_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.attachments"))
}

/// The content-addressed filename (`<sha256>.<ext>`) for an inline `data:`
/// URI. Single source of truth shared by the writer and the `cid -> filename`
/// map, so the recorded filename can never drift from the file on disk.
fn attachment_filename(data_url: &str) -> String {
    format!("{}.{}", sha256(data_url), extension_for_data_url(data_url))
}

/// Split a `data:<mime>;base64,<payload>` URI into (mime, payload).
fn split_data_url(data_url: &str) -> Option<(&str, &str)> {
    let rest = data_url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.split(';').next().unwrap_or("");
    Some((mime, payload))
}

/// Write the decoded bytes of an inline `data:` URI to the attachments dir as
/// `<sha256>.<ext>` and return the `cid:<sha256>` reference. Idempotent:
/// because the filename is the content hash, an identical blob maps to the
/// same path. The write uses `create_new` so concurrent callers can't race on
/// the same path — `AlreadyExists` means the (byte-identical) blob is already
/// stored, so it is a successful no-op. Non-`data:` URLs are returned
/// unchanged as their own reference (they are already external).
pub fn write_attachment(dir: &Path, data_url: &str) -> Result<String> {
    use std::io::Write;

    if !data_url.starts_with("data:") {
        return Ok(data_url.to_string());
    }
    let cid = cid_for_data_url(data_url);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create attachments dir {}", dir.display()))?;
    let file_path = dir.join(attachment_filename(data_url));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file_path)
    {
        Ok(mut file) => {
            let (_mime, payload) =
                split_data_url(data_url).ok_or_else(|| anyhow::anyhow!("malformed data URI"))?;
            let bytes = base64_decode(payload).context("failed to decode attachment base64")?;
            file.write_all(&bytes)
                .with_context(|| format!("Failed to write attachment {}", file_path.display()))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to create attachment {}", file_path.display()));
        }
    }
    Ok(cid)
}

/// Produces the on-the-wire representation of a `cid:` reference for an LLM
/// request. The base64 backend ships today; provider upload-API backends
/// (Anthropic Files, Gemini File API, …) are a planned follow-up.
pub trait AttachmentEncoder {
    /// Resolve a reference (`cid:<hash>`, an inline `data:` URI, or an external
    /// URL) into the value to send to the model.
    fn expand(&self, dir: &Path, reference: &str) -> Result<String>;
}

/// Re-inlines attachment bytes as a `data:` URI. Transient — the result is
/// placed in the outgoing request only, never re-stored.
pub struct Base64Encoder;

impl AttachmentEncoder for Base64Encoder {
    fn expand(&self, dir: &Path, reference: &str) -> Result<String> {
        let Some(hash) = reference.strip_prefix(CID_PREFIX) else {
            // Inline data URI or external URL — already wire-ready.
            return Ok(reference.to_string());
        };
        // Find the single file whose stem matches the hash.
        let entry = std::fs::read_dir(dir)
            .with_context(|| format!("attachments dir missing: {}", dir.display()))?
            .flatten()
            .find(|e| {
                e.path()
                    .file_stem()
                    .map(|s| s.to_string_lossy() == *hash)
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("attachment not found for {reference}"))?;
        let path = entry.path();
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        let mime = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "application/octet-stream",
        };
        let bytes = std::fs::read(&path)
            .with_context(|| format!("Failed to read attachment {}", path.display()))?;
        Ok(format!("data:{mime};base64,{}", base64_encode(bytes)))
    }
}

/// Rewrite every inline-`data:` image part in `parts` to a `cid:` reference,
/// writing the bytes to `dir` and recording `cid -> filename` in `map`.
/// Parts already holding a `cid:`/external URL are left unchanged.
pub fn externalize_parts(
    dir: &Path,
    parts: &mut [MessageContentPart],
    map: &mut HashMap<String, String>,
) -> Result<()> {
    for part in parts.iter_mut() {
        let MessageContentPart::ImageUrl { image_url } = part else {
            continue;
        };
        if !image_url.url.starts_with("data:") {
            continue;
        }
        let filename = attachment_filename(&image_url.url);
        let cid = write_attachment(dir, &image_url.url)?;
        map.insert(cid.clone(), filename);
        image_url.url = cid;
    }
    Ok(())
}

/// Replace every `cid:` image reference in `parts` with its inline `data:`
/// URI for transmission. Used on the send path only.
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
            Ok(url) => image_url.url = url,
            Err(err) => {
                // Never send a raw `cid:` ref to the model (it would be
                // rejected) and never fail the whole turn over one lost blob:
                // drop the unresolvable image to a text placeholder and keep
                // going with the rest.
                log::warn!("dropping unresolvable attachment {}: {err}", image_url.url);
                *part = MessageContentPart::Text {
                    text: format!("[unavailable image attachment: {}]", image_url.url),
                };
            }
        }
    }
    Ok(())
}

/// Remove a session's attachments directory if it exists. Safe to call when
/// the directory was never created.
pub fn remove_attachments_dir(session_yaml_path: &Path) -> Result<()> {
    let dir = attachments_dir_for(session_yaml_path);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        // No directory means there was nothing to clean up — a success, and
        // race-free (no exists()-then-remove gap).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("Failed to remove attachments dir {}", dir.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cid_is_stable_and_prefixed() {
        let url = "data:image/png;base64,QUJD";
        let cid = cid_for_data_url(url);
        assert!(cid.starts_with(CID_PREFIX));
        assert_eq!(cid, cid_for_data_url(url), "cid must be deterministic");
    }

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
    fn attachments_dir_is_sibling_of_transcript() {
        let p = std::path::Path::new("/tmp/sessions/agent/abc123.yaml");
        assert_eq!(
            attachments_dir_for(p),
            std::path::PathBuf::from("/tmp/sessions/agent/abc123.attachments")
        );
    }

    #[test]
    fn write_then_base64_expand_round_trips() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("s.attachments");
        // 3 bytes "ABC" base64 == "QUJD"
        let data_url = "data:image/png;base64,QUJD".to_string();

        let cid = write_attachment(&dir, &data_url).unwrap();
        assert!(cid.starts_with(CID_PREFIX));
        // The file is named <sha256>.<ext> inside the dir.
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "exactly one attachment file written");
        assert!(entries[0].file_name().to_string_lossy().ends_with(".png"));

        let encoder = Base64Encoder;
        let restored = encoder.expand(&dir, &cid).unwrap();
        assert_eq!(
            restored, data_url,
            "expand reproduces the original data URI"
        );

        // Writing the same blob again is idempotent (no duplicate file).
        let cid2 = write_attachment(&dir, &data_url).unwrap();
        assert_eq!(cid, cid2);
        let count = std::fs::read_dir(&dir).unwrap().flatten().count();
        assert_eq!(count, 1, "duplicate blob does not create a second file");
    }

    #[test]
    fn remove_attachments_dir_is_idempotent() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let yaml = tmp.path().join("abc.yaml");
        let dir = attachments_dir_for(&yaml);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.png"), b"x").unwrap();

        remove_attachments_dir(&yaml).unwrap();
        assert!(!dir.exists());
        // Second call is a no-op, not an error.
        remove_attachments_dir(&yaml).unwrap();
    }

    #[test]
    fn expand_parts_drops_unresolvable_cid() {
        use harnx_core::message::{ImageUrl, MessageContentPart};
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("missing.attachments"); // never created
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
        use harnx_core::message::{ImageUrl, MessageContentPart};
        use std::collections::HashMap;
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

        // The image part now holds a cid: reference, not the base64.
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
            "recorded filename carries the image extension"
        );

        // Expansion restores the data URI in-place.
        let encoder = Base64Encoder;
        expand_parts(&encoder, &dir, &mut parts).unwrap();
        match &parts[1] {
            MessageContentPart::ImageUrl { image_url } => assert_eq!(image_url.url, data_url),
            other => panic!("expected ImageUrl, got {other:#?}"),
        }
        // Non-image parts are left untouched throughout.
        match &parts[0] {
            MessageContentPart::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected Text, got {other:#?}"),
        }
    }
}
