//! Error type for specification loading, normalization and catalog building.
//!
//! Every variant carries the location the problem was found at — the whole
//! document, an `OPERATION /path` pair, or the `$ref` that could not be
//! resolved — so a failed catalog build names what to fix instead of pointing
//! at a blob of JSON.

use thiserror::Error;

/// A specification could not be loaded, validated, normalized or cataloged.
///
/// Catalog building is all-or-nothing: a [`SpecError`](self) means no tools are
/// registered from the document, never a partial catalog that silently misses
/// operations.
#[derive(Debug, Error)]
pub enum SpecError {
    /// The document uses a feature this crate deliberately does not support
    /// (OpenAPI 3.1, external references, callbacks, unsupported parameter
    /// serialization, cookie parameters, ...).
    #[error("unsupported specification at {context}: {message}")]
    Unsupported { context: String, message: String },

    /// The document is structurally broken: a missing path parameter, an
    /// operation without media entries, a `path` that is not an object, ...
    #[error("invalid specification at {context}: {message}")]
    Invalid { context: String, message: String },

    /// A local `$ref` points at a location that does not exist (or a schema
    /// cycles without going through a named definition we can keep).
    #[error("unresolved reference {reference} at {context}: {message}")]
    UnresolvedRef {
        context: String,
        reference: String,
        message: String,
    },

    /// An operator-supplied source patch (applied to the parsed document
    /// root before normalization) failed. The message names the patch by
    /// one-based index and a failure category only — never the expression
    /// text, document content, or the underlying jaq error chain.
    #[error("spec patch failed: {0}")]
    SpecPatch(String),

    /// The operator-supplied configuration (server name, prefix budget) is
    /// unusable, independent of the specification content.
    #[error("configuration error: {0}")]
    Config(String),

    /// Reading the specification file failed.
    #[error("failed to read specification: {0}")]
    Io(#[from] std::io::Error),
}

impl SpecError {
    pub(crate) fn unsupported(context: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Unsupported {
            context: context.into(),
            message: message.into(),
        }
    }

    pub(crate) fn invalid(context: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invalid {
            context: context.into(),
            message: message.into(),
        }
    }

    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub(crate) fn spec_patch(message: impl Into<String>) -> Self {
        Self::SpecPatch(message.into())
    }
}

/// Shorthand result type for normalization.
pub type SpecResult<T> = Result<T, SpecError>;
