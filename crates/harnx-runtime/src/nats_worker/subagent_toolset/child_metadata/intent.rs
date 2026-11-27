//! The durable creation/existing decision for a patch-bearing call.

use super::guards::{guard_of, MetadataGuard};
use super::{recoverable, SubagentToolset};
use harnx_toolset::ToolInvokeError;
use serde_json::Value;

pub(super) enum PatchIntent {
    /// The record is this invocation's own creation; its first record
    /// already carries the requested state. Never reapply, never reset what
    /// the record gained since.
    Bypass,
    /// Create the missing record; `pinned` says the creation guard is
    /// already in the checkpoint.
    Create { pinned: bool },
    /// Patch the existing record at exactly this revision; [`ExistingPin`]
    /// says whether the first-writer checkpoint already pins it.
    Existing(ExistingPin),
    /// A pinned creation guard found a record another creator owns: a
    /// recoverable conflict, never a switch to existing-session mutation.
    CompetingCreation,
    /// A revision guard names a record that no longer exists.
    GuardWithoutRecord,
    /// The saved checkpoint predates the guard fields; refuse to retrofit a
    /// revision pin onto a legacy checkpoint.
    LegacyCheckpoint,
}

/// The revision an existing-record patch must apply at, and whether the
/// first-writer checkpoint already pins it.
pub(super) enum ExistingPin {
    /// The checkpoint already carries this revision guard.
    Pinned(u64),
    /// The guard still has to be pinned at this observed revision.
    Fresh(u64),
}

pub(super) async fn patch_intent(
    toolset: &SubagentToolset,
    storage: &str,
    call_id: &str,
    saved_checkpoint: Option<&Value>,
) -> Result<PatchIntent, ToolInvokeError> {
    let record = toolset
        .session_metadata
        .get(storage)
        .await
        .map_err(recoverable)?;
    let ours = record
        .as_ref()
        .and_then(|record| record.metadata.creation_invocation.as_deref())
        == Some(call_id);
    match (guard_of(saved_checkpoint), record) {
        // Our own creation (generated reservation or explicit create): the
        // requested state landed in the first canonical record.
        (_, Some(_)) if ours => Ok(PatchIntent::Bypass),
        (Some(MetadataGuard::Creation), None) => Ok(PatchIntent::Create { pinned: true }),
        (Some(MetadataGuard::Creation), Some(_)) => Ok(PatchIntent::CompetingCreation),
        (Some(MetadataGuard::Revision(revision)), Some(_)) => {
            Ok(PatchIntent::Existing(ExistingPin::Pinned(revision)))
        }
        (Some(MetadataGuard::Revision(_)), None) => Ok(PatchIntent::GuardWithoutRecord),
        (None, None) => Ok(PatchIntent::Create { pinned: false }),
        (None, Some(record)) => {
            if saved_checkpoint.is_some() {
                Ok(PatchIntent::LegacyCheckpoint)
            } else {
                Ok(PatchIntent::Existing(ExistingPin::Fresh(record.revision)))
            }
        }
    }
}
