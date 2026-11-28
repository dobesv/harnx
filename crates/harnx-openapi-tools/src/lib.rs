//! harnx-openapi-tools: Swagger 2.0 / OpenAPI 3.0 normalization and compact
//! tool catalogs.
//!
//! Pure normalization: no network I/O, no authentication, no toolset hosting.
//! Load a local specification ([`document::SpecDocument`]), normalize it into
//! a [`catalog::Catalog`] for a configured server name, and read either the
//! compact discovery list ([`Catalog::tool_specs`]) or the dispatch table
//! ([`Catalog::operations`]).
//!
//! Supported: Swagger 2.0 and OpenAPI 3.0.x documents (JSON or YAML) with
//! local references, including recursive definitions. OpenAPI 3.1, external
//! references, cookie parameters and exotic serialization styles are rejected
//! with actionable, located diagnostics — an operation is never silently
//! missing.

pub mod catalog;
pub mod compact;
pub mod document;
pub mod envelope;
pub mod error;
pub mod media;
pub mod naming;
pub mod normalize;
pub mod refs;
pub mod security;
pub mod select;
pub mod spec_patch;

pub use catalog::Catalog;
pub use document::{SpecDocument, SpecKind};
pub use envelope::BodyInput;
pub use error::{SpecError, SpecResult};
pub use media::{json_patch_array_schema, select_media_branch, MediaBranch, MediaKind};
pub use naming::{Namer, MAX_MODEL_TOOL_NAME_LEN, MIN_RAW_NAME_BUDGET};
pub use normalize::{NormalizedOperation, ParamSpec, ParamStyle};
pub use security::{ApiKeyPlacement, ProtectedParam, SecuritySchemes, RESERVED_HEADER_SCHEME};
pub use select::{
    build_catalog_with_controls, select_operations, OperationSelection, OperationSelector,
};
pub use spec_patch::{
    apply_spec_patches, validate_spec_patches, SpecPatchLimits, MAX_SPEC_PATCH_COUNT,
    MAX_SPEC_PATCH_DOCUMENT_BYTES, MAX_SPEC_PATCH_DOCUMENT_DEPTH, MAX_SPEC_PATCH_EXPRESSION_BYTES,
};
