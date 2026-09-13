//! Canonical NATS KV storage for session identity, configuration, and activity.
//!
//! Session transcripts contain conversation events only. Everything required
//! to identify and rehydrate a session lives under `sessions/{id}/meta`, while
//! frequently refreshed lifecycle timestamps live under
//! `sessions/{id}/activity` so lease renewal does not contend with metadata
//! mutations.
//!
//! Read-state for session unread tracking lives under `sessions/{id}/read/default`
//! with a dedicated invalidation subject `harnx.session.{id}.read.invalidated`.

mod activity;
mod execution_context;
mod initializer;
mod model;
mod read_state;
mod store;
mod tool_context;
mod view;

pub const SESSION_METADATA_BUCKET: &str = "harnx_sessions";
pub const SESSION_METADATA_SCHEMA_VERSION: u32 = 1;
pub const EXTENSION_NAMESPACE_MAX_BYTES: usize = 64 * 1024;
pub const EXTENSIONS_TOTAL_MAX_BYTES: usize = 256 * 1024;
const CAS_RETRY_LIMIT: usize = 8;

pub use activity::SessionActivity;
pub use execution_context::execution_contexts;
pub use initializer::SessionInitializer;
pub use model::{
    SessionAgentSource, SessionMetadata, SessionOverrideUpdate, SessionOverrides, SessionTitle,
};
pub use read_state::SessionReadState;
pub use store::{
    activity_key, invalidation_subject, metadata_key, read_cursor_key, read_invalidation_subject,
    session_prefix, SessionExtensionUpdate, SessionMetadataStore,
};
pub use tool_context::{
    tool_context, ToolContext, ToolContextEntry, TOOL_CONTEXT_NAMESPACE, TOOL_CONTEXT_VERSION,
};
pub use view::{
    ListedSession, MetadataRecord, RedactedAgentSource, RedactedRepositoryContext,
    RedactedSessionMetadata, SessionMetadataPatch, SessionTitlePatch, VariableStatus,
};

#[cfg(test)]
mod tests;
