//! Content-addressed storage for image/binary attachments referenced from
//! session transcripts. Blobs live in a per-session `{id}.attachments/`
//! directory and are referenced from message content as `cid:<sha256>`,
//! keeping multi-megabyte base64 out of the transcript and out of memory at
//! rest. The wire encoding is pluggable (`AttachmentEncoder`); only the
//! base64 backend ships today, with provider upload-API backends to follow.

use std::path::{Path, PathBuf};

use harnx_core::crypto::sha256;

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
        assert_eq!(extension_for_data_url("data:application/x;base64,AA"), "bin");
    }

    #[test]
    fn attachments_dir_is_sibling_of_transcript() {
        let p = std::path::Path::new("/tmp/sessions/agent/abc123.yaml");
        assert_eq!(
            attachments_dir_for(p),
            std::path::PathBuf::from("/tmp/sessions/agent/abc123.attachments")
        );
    }
}
