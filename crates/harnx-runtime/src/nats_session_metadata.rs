//! Canonical NATS KV storage for session identity, configuration, and activity.
//!
//! Session transcripts contain conversation events only. Everything required
//! to identify and rehydrate a session lives under `sessions/{storage_key}/meta`,
//! while frequently refreshed lifecycle timestamps live under
//! `sessions/{storage_key}/activity` so lease renewal does not contend with
//! metadata mutations. `storage_key` is the SHA-256 identity derived from agent
//! plus local session ID.
//!
//! Read-state for session unread tracking lives under
//! `sessions/{storage_key}/read/default` with a dedicated invalidation subject
//! `harnx.session.{storage_key}.read.invalidated`.

mod activity;
mod admission;
pub use admission::{AdmissionAuthority, AdmissionOrigin, InvocationAdmission};
mod execution_context;
mod initializer;
mod model;
mod read_state;
pub mod run_limits;
mod session_properties;
mod store;
mod tool_context;
mod view;

pub const SESSION_METADATA_BUCKET: &str = "harnx_sessions";
pub const SESSION_METADATA_SCHEMA_VERSION: u32 = 1;
pub const EXTENSION_NAMESPACE_MAX_BYTES: usize = 64 * 1024;
pub const EXTENSIONS_TOTAL_MAX_BYTES: usize = 256 * 1024;
pub const CAS_RETRY_LIMIT: usize = 8;

pub use activity::SessionActivity;
pub use execution_context::execution_contexts;
pub use initializer::SessionInitializer;
pub use model::{
    ParentLink, SessionAgentSource, SessionMetadata, SessionOverrideUpdate, SessionOverrides,
    SessionTitle,
};
pub use read_state::SessionReadState;
pub use run_limits::{
    CallTimeoutOverride, EffectiveDeadline, InvocationEdgeKind, InvocationIdentity,
    ParentInvocationLink, RunIdentity, RunLimitsError, RunLimitsPolicySource, RunLimitsRecord,
};
pub use session_properties::{
    property_definition, session_properties, Inheritance, PropertyAssignment, PropertyDefinition,
    PropertySource, SessionProperties, SessionPropertiesUpdate, SessionProperty,
    CUSTOM_PROPERTIES_MAX, LABELS_PROPERTY, PROPERTY_DEFINITIONS, SESSION_LABELS_MAX,
    SESSION_PROPERTIES_NAMESPACE, WEB_SESSION_URL_PROPERTY,
};
pub use store::{
    a2a_message_key, a2a_session_prefix, a2a_task_index_key, a2a_task_key, a2a_tasks_prefix,
    activity_key, invalidation_subject, is_cas_conflict, metadata_belongs_to_agent, metadata_key,
    read_cursor_key, read_invalidation_subject, session_prefix, SessionExtensionUpdate,
    SessionMetadataStore, TaskIndex, TaskIndexEntry, TaskState,
};
pub use tool_context::{
    tool_context, ToolContext, ToolContextEntry, TOOL_CONTEXT_NAMESPACE, TOOL_CONTEXT_VERSION,
};
pub use view::{
    repository_contexts, ListedSession, MetadataRecord, RedactedAgentSource,
    RedactedRepositoryContext, RedactedSessionMetadata, SessionMetadataPatch, SessionTitlePatch,
    VariableStatus,
};

#[cfg(test)]
mod tests;
