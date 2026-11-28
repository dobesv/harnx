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

use crate::compact::is_binary_schema;
use crate::error::{SpecError, SpecResult};

mod matching;
use matching::{classify_concrete, is_concrete, is_token};
pub(crate) use matching::{encoding_domains, excluding, MediaMatch};

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
    /// The envelope `encoding` discriminator for a concrete media kind.
    pub fn encoding(self) -> Option<&'static str> {
        match self {
            Self::Json | Self::JsonPatch => Some("json"),
            Self::Text => Some("text"),
            Self::UrlencodedForm => Some("form"),
            Self::Multipart => Some("multipart"),
            Self::Base64 => Some("base64"),
            Self::Wildcard => None,
        }
    }
}

/// Classify a media declaration by its canonical base type.
fn classify_media(media_type: &str) -> MediaKind {
    if matches!(MediaMatch::new(media_type), MediaMatch::Exact(_)) {
        classify_concrete(media_type)
    } else {
        MediaKind::Wildcard
    }
}

fn canonical_base(media_type: &str) -> String {
    let base = media_type
        .split_once(';')
        .map_or(media_type, |(base, _)| base);
    base.trim().to_ascii_lowercase()
}

fn validate_media_schema(media_type: &str, schema: &Value, context: &str) -> SpecResult<()> {
    if schema.is_null() {
        return Err(SpecError::invalid(
            context,
            format!("media entry {media_type:?} has a null schema"),
        ));
    }
    Ok(())
}

fn validated_media_type(media_type: &str, context: &str) -> SpecResult<String> {
    validate_canonical_media_type(media_type, canonical_base(media_type), context)
}

fn validate_canonical_media_type(
    original: &str,
    canonical: String,
    context: &str,
) -> SpecResult<String> {
    if media_type_is_valid(&canonical) {
        Ok(canonical)
    } else {
        Err(SpecError::invalid(
            context,
            format!("invalid advertised media type {original:?}"),
        ))
    }
}

fn media_type_is_valid(media_type: &str) -> bool {
    media_match_is_valid(MediaMatch::new(media_type))
}

fn media_match_is_valid(media: MediaMatch<'_>) -> bool {
    match media {
        MediaMatch::Any => true,
        MediaMatch::Type(ty) => is_token(ty),
        MediaMatch::Exact(ty) => is_concrete(ty),
    }
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
    /// Canonical advertised type or range: lowercase, without parameters.
    pub media_type: String,
    pub kind: MediaKind,
    /// The compacted source schema of the entry, `{}` when the document
    /// declares none.
    pub source_schema: Value,
    /// Raw OAS3 per-property `encoding` map for multipart entries, kept for
    /// dispatch.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub encoding: Map<String, Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    binary_wildcard: bool,
}

impl MediaBranch {
    pub fn new(
        media_type: &str,
        source_schema: Value,
        encoding: Map<String, Value>,
        context: &str,
    ) -> SpecResult<Self> {
        validate_media_schema(media_type, &source_schema, context)?;
        let canonical = validated_media_type(media_type, context)?;
        Ok(Self::from_canonical(canonical, source_schema, encoding))
    }

    fn from_canonical(
        media_type: String,
        source_schema: Value,
        encoding: Map<String, Value>,
    ) -> Self {
        let mut branch = Self {
            kind: classify_media(&media_type),
            media_type,
            source_schema,
            encoding,
            binary_wildcard: false,
        };
        branch.resolve_payload_kinds(&Map::new());
        branch
    }

    pub(crate) fn resolve_payload_kinds(&mut self, defs: &Map<String, Value>) {
        self.binary_wildcard =
            self.kind == MediaKind::Wildcard && is_binary_schema(&self.source_schema, defs);
    }

    pub(crate) fn matcher(&self) -> MediaMatch<'_> {
        MediaMatch::new(&self.media_type)
    }

    pub(crate) fn allows_kind(&self, kind: MediaKind) -> bool {
        !self.binary_wildcard || !matches!(kind, MediaKind::Json | MediaKind::JsonPatch)
    }

    /// Concrete encoder for this branch and media type, if compatible. Call
    /// after selecting the branch; do not retry with a less specific branch.
    pub fn payload_kind(&self, content_type: &str) -> Option<MediaKind> {
        if !is_concrete(content_type) || !self.matcher().covers(content_type) {
            return None;
        }
        let kind = classify_concrete(content_type);
        self.allows_kind(kind).then_some(kind)
    }
}

// qual:api — media selection contract used by T3.
/// Pick the branch for a lowercase, parameter-free, concrete `content_type`.
/// Media parameters (including multipart boundaries) are encoder-owned.
/// Exact entries win over `type/*`, then `*/*`. If the winning entry cannot
/// encode this concrete type, there is no match; never use a weaker branch.
pub fn select_media_branch<'a>(
    branches: &'a [MediaBranch],
    content_type: &str,
) -> Option<&'a MediaBranch> {
    if !is_concrete(content_type) {
        return None;
    }
    branches
        .iter()
        .filter(|branch| branch.matcher().covers(content_type))
        .max_by_key(|branch| branch.matcher().precedence())
        .filter(|branch| branch.payload_kind(content_type).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn branch(media_type: &str, schema: Value) -> MediaBranch {
        MediaBranch::new(media_type, schema, Map::new(), "POST /test").unwrap()
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
        let selected = select_media_branch(&branches, "application/json").unwrap();
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
