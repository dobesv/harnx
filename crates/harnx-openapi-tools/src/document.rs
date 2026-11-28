//! Parsed specification document: version detection and local loading.
//!
//! Supports Swagger 2.0 and OpenAPI 3.0.x from JSON or YAML. OpenAPI 3.1 and
//! everything else are rejected with an actionable message. Local-file loading
//! is synchronous and never touches the network; fetching a `--spec-url` is a
//! startup concern owned by the server (T3), which hands the fetched bytes to
//! [`SpecDocument::parse`] after its network policy checks.

use std::path::Path;

use serde_json::Value;

use crate::error::{SpecError, SpecResult};

/// Which dialect a document declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecKind {
    /// `swagger: "2.0"`.
    Swagger2,
    /// `openapi: "3.0.x"`.
    OpenApi3,
}

/// A parsed specification, kept as the raw JSON value tree.
///
/// Order of paths, media entries and map keys is preserved (the workspace
/// enables `serde_json/preserve_order`), which is what makes catalog names
/// deterministic for a given document.
#[derive(Clone, Debug)]
pub struct SpecDocument {
    pub kind: SpecKind,
    /// The raw `swagger` / `openapi` version string.
    pub version: String,
    pub root: Value,
}

impl SpecDocument {
    /// Classify an already-parsed document tree.
    pub fn from_value(root: Value) -> SpecResult<Self> {
        let openapi = root.get("openapi").and_then(Value::as_str);
        let swagger = root.get("swagger").and_then(Value::as_str);
        let (kind, version) = match (openapi, swagger) {
            (Some(version), _) if version.starts_with("3.1") => {
                return Err(SpecError::unsupported(
                    "document",
                    format!(
                        "OpenAPI {version} is not supported; this server normalizes Swagger 2.0 \
                         and OpenAPI 3.0.x only. Re-export the document as OpenAPI 3.0."
                    ),
                ));
            }
            (Some(version), _) if version.starts_with("3.0") => (SpecKind::OpenApi3, version),
            (Some(version), _) => {
                return Err(SpecError::unsupported(
                    "document",
                    format!(
                        "unknown OpenAPI version {version:?}; supported versions are Swagger 2.0 \
                         and OpenAPI 3.0.x"
                    ),
                ));
            }
            (None, Some("2.0")) => (SpecKind::Swagger2, "2.0"),
            (None, Some(version)) => {
                return Err(SpecError::unsupported(
                    "document",
                    format!(
                        "unknown Swagger version {version:?}; supported versions are Swagger 2.0 \
                         and OpenAPI 3.0.x"
                    ),
                ));
            }
            (None, None) => {
                return Err(SpecError::unsupported(
                    "document",
                    "neither an `openapi` (3.0.x) nor a `swagger: \"2.0\"` field was found; this \
                     is not a Swagger 2.0 or OpenAPI 3.0 document",
                ));
            }
        };
        if !root.get("paths").map(Value::is_object).unwrap_or(false) {
            return Err(SpecError::invalid(
                "document",
                "the `paths` field is missing or not an object",
            ));
        }
        Ok(Self {
            kind,
            version: version.to_string(),
            root,
        })
    }

    // qual:allow(iosp) — two-way JSON/YAML dispatch: the branch decision and
    // the two parser calls cannot be separated without hiding the dispatch.
    /// Parse specification bytes. JSON (leading `{`) and YAML are both
    /// accepted; YAML documents produce the same value tree.
    pub fn parse(bytes: &[u8]) -> SpecResult<Self> {
        let leading = bytes
            .iter()
            .find(|b| !b.is_ascii_whitespace())
            .copied()
            .unwrap_or(0);
        if leading == b'{' {
            Self::from_json(bytes)
        } else {
            Self::from_yaml(bytes)
        }
    }

    /// Parse a JSON specification.
    pub fn from_json(bytes: &[u8]) -> SpecResult<Self> {
        let root: Value = serde_json::from_slice(bytes).map_err(|error| {
            SpecError::invalid("document", format!("invalid JSON specification: {error}"))
        })?;
        Self::from_value(root)
    }

    /// Parse a YAML specification.
    pub fn from_yaml(bytes: &[u8]) -> SpecResult<Self> {
        let root: Value = serde_yaml::from_slice(bytes).map_err(|error| {
            SpecError::invalid("document", format!("invalid YAML specification: {error}"))
        })?;
        Self::from_value(root)
    }

    // qual:api — local file loading is the documented entry point for the
    // tool server (T3/T4) and the test fixtures.
    /// Read and parse a local specification file. Never contacts the network.
    pub fn from_path(path: &Path) -> SpecResult<Self> {
        let bytes = std::fs::read(path)?;
        Self::parse(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SpecError;

    fn parse(text: &str) -> SpecResult<SpecDocument> {
        SpecDocument::parse(text.as_bytes())
    }

    const MINIMAL_SWAGGER: &str = r#"{"swagger": "2.0", "paths": {}}"#;
    const MINIMAL_OAS3: &str = r#"{"openapi": "3.0.1", "paths": {}}"#;

    #[test]
    fn detects_swagger2_and_oas3() {
        harnx_core::require_nextest();
        let swagger = parse(MINIMAL_SWAGGER).unwrap();
        assert_eq!(swagger.kind, SpecKind::Swagger2);
        let oas3 = parse(MINIMAL_OAS3).unwrap();
        assert_eq!(oas3.kind, SpecKind::OpenApi3);
    }

    #[test]
    fn accepts_yaml_documents() {
        harnx_core::require_nextest();
        let document = parse("swagger: \"2.0\"\npaths: {}\n").unwrap();
        assert_eq!(document.kind, SpecKind::Swagger2);
    }

    #[test]
    fn rejects_openapi_3_1_with_guidance() {
        harnx_core::require_nextest();
        let error = parse(r#"{"openapi": "3.1.0", "paths": {}}"#).unwrap_err();
        let SpecError::Unsupported { message, .. } = error else {
            panic!("expected Unsupported, got {error:?}");
        };
        assert!(message.contains("3.1"), "{message}");
        assert!(message.contains("3.0"), "{message}");
    }

    #[test]
    fn rejects_missing_paths_and_unknown_versions() {
        harnx_core::require_nextest();
        assert!(parse(r#"{"openapi": "4.0.0"}"#).is_err());
        assert!(parse(r#"{"swagger": "1.2"}"#).is_err());
        assert!(parse(r#"{"info": {}}"#).is_err());
        assert!(parse(r#"{"openapi": "3.0.1"}"#).is_err());
    }
}
