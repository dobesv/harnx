//! Media-type classification and selection precedence.
//!
//! Reviewed contract: an exact advertised media entry wins over matching
//! type/subtype wildcards, then `*/*`. JSON and `+json` select JSON values;
//! `application/json-patch+json` selects an array of RFC 6902 patch operations
//! even when the source schema wrongly says `object`; `text/*` selects text;
//! `application/x-www-form-urlencoded` selects ordered `{name, value}` string
//! pairs; `multipart/*` selects ordered named parts; every other concrete type
//! selects base64 bytes. A wildcard entry offers concrete media selection
//! with these generic encoders — including URL-encoded fields through `*/*` —
//! but never raw HTTP framing control.
//!
//! Envelope construction for multi-media bodies lives in [`crate::envelope`].

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// How a request-body media entry encodes onto the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// JSON values, including any structured-syntax `+json` suffix.
    Json,
    /// RFC 6902 JSON Patch arrays.
    JsonPatch,
    /// Text bodies.
    Text,
    /// URL-encoded form fields.
    UrlencodedForm,
    /// Multipart named parts.
    Multipart,
    /// Raw bytes as base64.
    Base64,
    /// A wildcard entry (`*/*` or `type/*`) offering generic encoders with
    /// concrete media selection.
    Wildcard,
}

impl MediaKind {
    /// The envelope `encoding` discriminator values this kind makes available.
    pub fn encodings(self) -> &'static [&'static str] {
        match self {
            Self::Json | Self::JsonPatch => &["json"],
            Self::Text => &["text"],
            Self::UrlencodedForm => &["form"],
            Self::Multipart => &["multipart"],
            Self::Base64 => &["base64"],
            Self::Wildcard => &["json", "text", "form", "multipart", "base64"],
        }
    }
}

/// Classify an advertised media type.
pub fn classify_media(media_type: &str) -> MediaKind {
    let media = media_type.trim().to_ascii_lowercase();
    if media == "application/json-patch+json" {
        return MediaKind::JsonPatch;
    }
    if media == "application/json" || media.ends_with("+json") {
        return MediaKind::Json;
    }
    if media.starts_with("text/") {
        return MediaKind::Text;
    }
    if media == "application/x-www-form-urlencoded" {
        return MediaKind::UrlencodedForm;
    }
    if media.starts_with("multipart/") {
        return MediaKind::Multipart;
    }
    if media == "*/*" || media.ends_with("/*") {
        return MediaKind::Wildcard;
    }
    MediaKind::Base64
}

/// The published schema for the RFC 6902 patch array a `json-patch+json`
/// entry selects, regardless of a contradictory source schema.
pub fn json_patch_array_schema() -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "items": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "op": {"type": "string", "enum": ["add", "remove", "replace", "move", "copy", "test"]},
                "path": {"type": "string"},
                "from": {"type": "string", "description": "Source location for move/copy operations."},
                "value": {}
            },
            "required": ["op", "path"]
        }
    })
}

/// One advertised request-body media entry, with its compacted source schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaBranch {
    /// The media type as advertised in the document (`application/json`,
    /// `*/*`, ...). Wildcard entries are kept verbatim.
    pub media_type: String,
    pub kind: MediaKind,
    /// The compacted source schema of the entry, `{}` when the document
    /// declares none.
    pub source_schema: Value,
    /// Raw OAS3 per-property `encoding` map for multipart entries, kept for
    /// dispatch.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub encoding: Map<String, Value>,
}

impl MediaBranch {
    pub fn new(media_type: &str, source_schema: Value, encoding: Map<String, Value>) -> Self {
        Self {
            media_type: media_type.to_string(),
            kind: classify_media(media_type),
            source_schema,
            encoding,
        }
    }

    pub fn is_wildcard(&self) -> bool {
        self.kind == MediaKind::Wildcard
    }
}

// qual:api — media selection is the reviewed dispatch contract used by T3
// and pinned by tests; see the module docs for the precedence rules.
/// Pick the branch a concrete `content_type` selects.
///
/// Precedence: an exact advertised entry, then a matching `type/*` entry, then
/// `*/*`. Wildcard content types and values with parameters are never matched
/// against exact entries.
pub fn select_media_branch<'a>(
    branches: &'a [MediaBranch],
    content_type: &str,
) -> Option<&'a MediaBranch> {
    let concrete = normalize_media(content_type)?;
    if concrete.contains('*') {
        return None;
    }
    if let Some(branch) = branches.iter().find(|branch| {
        !branch.is_wildcard()
            && normalize_media(&branch.media_type).as_deref() == Some(concrete.as_str())
    }) {
        return Some(branch);
    }
    let (selected_type, _) = concrete.split_once('/')?;
    let type_wildcard = format!("{selected_type}/*");
    branches
        .iter()
        .find(|branch| branch.is_wildcard() && branch.media_type == type_wildcard)
        .or_else(|| branches.iter().find(|branch| branch.media_type == "*/*"))
}

fn normalize_media(media_type: &str) -> Option<String> {
    let base = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base.split_once('/').is_some() && !base.contains(' ') {
        Some(base)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn branch(media_type: &str, schema: Value) -> MediaBranch {
        MediaBranch::new(media_type, schema, Map::new())
    }

    #[test]
    fn classifies_advertised_media() {
        harnx_core::require_nextest();
        assert_eq!(classify_media("application/json"), MediaKind::Json);
        assert_eq!(classify_media("application/vnd.api+json"), MediaKind::Json);
        assert_eq!(
            classify_media("application/json-patch+json"),
            MediaKind::JsonPatch
        );
        assert_eq!(classify_media("text/plain"), MediaKind::Text);
        assert_eq!(
            classify_media("application/x-www-form-urlencoded"),
            MediaKind::UrlencodedForm
        );
        assert_eq!(classify_media("multipart/form-data"), MediaKind::Multipart);
        assert_eq!(
            classify_media("application/octet-stream"),
            MediaKind::Base64
        );
        assert_eq!(classify_media("*/*"), MediaKind::Wildcard);
        assert_eq!(classify_media("image/*"), MediaKind::Wildcard);
    }

    #[test]
    fn selection_prefers_exact_then_type_wildcard_then_any() {
        harnx_core::require_nextest();
        let branches = vec![
            branch("application/json", json!({})),
            branch("image/*", json!({})),
            branch("*/*", json!({})),
        ];
        let selected = select_media_branch(&branches, "application/json; charset=utf-8").unwrap();
        assert_eq!(selected.media_type, "application/json");
        assert_eq!(
            select_media_branch(&branches, "image/png")
                .unwrap()
                .media_type,
            "image/*"
        );
        assert_eq!(
            select_media_branch(&branches, "text/csv")
                .unwrap()
                .media_type,
            "*/*"
        );
        assert!(select_media_branch(&branches, "*/*").is_none());
        assert!(select_media_branch(&branches, "not-a-media-type").is_none());
    }
}
