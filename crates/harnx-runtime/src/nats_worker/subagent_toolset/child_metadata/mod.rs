//! Admission for a parent-supplied child metadata patch on the session
//! tools: the durable gate, the first-writer checkpoint guards and the
//! creation/existing decision.
//!
//! Safety rules (plan #2970, T2), all enforced here so `session_prompt`,
//! `session_new` and `session_send` cannot drift:
//! - only durable admission proof (an accepted head, an accepted busy-join
//!   record or binding, or terminal proof for this invocation) skips the
//!   gate — a saved candidate admission is not proof;
//! - a fresh or candidate-only call establishes the guard BEFORE any
//!   mutation: the invocation checkpoint pins the record revision for
//!   existing sessions, a creation guard for explicit-ID creates, and the
//!   reservation's atomic `creation_invocation` stamp for generated creates
//!   (no second protocol);
//! - a retried call never resets later child edits: our own creation record
//!   bypasses, an already-held patch proceeds without writing, and a
//!   revision conflict is a recoverable error after an admission recheck,
//!   never a rebase;
//! - a checkpoint written without a guard is never retrofitted.

mod admission;
mod claim;
mod context;
mod guards;
mod intent;
mod load_page;
mod read;
mod replay;
mod schema;
mod settle;

pub(super) use admission::fresh_send_composes_content;
pub(super) use claim::verify_creation_claim;
pub(in crate::nats_worker::subagent_toolset) use context::{
    saved_checkpoint_scope, validate_sync_saved_scope, InheritedPrompt, ScopeMismatch,
};
pub(super) use load_page::{compose_load_page, LoadScope};
pub(super) use read::{load_metadata, metadata_allowance};
pub(super) use replay::{sync_replay_check, ReplayCheck};
pub(super) use schema::metadata_object_schema;
pub(super) use settle::{prepare_send, prepare_sync};

use super::SubagentToolset;
use crate::nats_session_metadata::ChildSessionMetadataPatch;
use harnx_toolset::{ToolInvocationContext, ToolInvokeError};

/// Creation metadata threaded into the reservation/create initializer so the
/// first canonical record already carries the requested state, stamped with
/// the trusted invocation identity.
pub(super) struct ChildCreation {
    pub(super) patch: ChildSessionMetadataPatch,
    pub(super) invocation: String,
}

impl ChildCreation {
    fn new(patch: ChildSessionMetadataPatch, invocation: &str) -> Self {
        Self {
            patch,
            invocation: invocation.to_string(),
        }
    }
}

/// What a patch-bearing call still has to do before entering the executable
/// append path.
pub(super) enum MetadataPlan {
    /// Nothing: no patch, admitted replay, or the patch already holds.
    Settled,
    /// Create the record with the patch in the first canonical record.
    /// Boxed: the patch carries the full variables map and property update.
    Create(Box<ChildCreation>),
}

impl MetadataPlan {
    pub(super) fn creation(&self) -> Option<&ChildCreation> {
        match self {
            Self::Settled => None,
            Self::Create(creation) => Some(creation),
        }
    }
}

/// The `metadata` argument after strict parsing: `None` when omitted or
/// empty, otherwise pre-flighted so an invalid patch fails before any
/// reservation or mutation.
pub(super) fn extract(
    metadata: Option<ChildSessionMetadataPatch>,
) -> Result<Option<ChildSessionMetadataPatch>, ToolInvokeError> {
    let Some(patch) = metadata else {
        return Ok(None);
    };
    if patch.is_empty() {
        return Ok(None);
    }
    patch.validate().map_err(|error| {
        ToolInvokeError::Recoverable(format!("invalid child session metadata: {error:#}"))
    })?;
    Ok(Some(patch))
}

/// The session id a saved invocation checkpoint names, if any.
pub(super) fn checkpoint_session_id(context: &ToolInvocationContext) -> Option<String> {
    context
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint["session_id"].as_str())
        .map(str::to_string)
}

fn storage_key(toolset: &SubagentToolset, session_id: &str) -> String {
    harnx_core::session_identity::session_key(Some(&toolset.agent), session_id)
}

fn recoverable(error: anyhow::Error) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!("child session metadata: {error:#}"))
}
