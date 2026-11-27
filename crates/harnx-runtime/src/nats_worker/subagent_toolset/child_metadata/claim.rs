//! The create claim check for a patch-bearing creation.

use crate::nats_session::SessionMetadataClaim;
use harnx_toolset::ToolInvokeError;

/// Verify the create claim a patch-bearing create produced: our own creation
/// record proceeds, a competing creator is a recoverable conflict without
/// dispatch.
pub(in crate::nats_worker::subagent_toolset) fn verify_creation_claim(
    claim: &SessionMetadataClaim,
    invocation: &str,
    session_id: &str,
) -> Result<(), ToolInvokeError> {
    match claim {
        SessionMetadataClaim::Created(_) => Ok(()),
        SessionMetadataClaim::Existing(record)
            if record.metadata.creation_invocation.as_deref() == Some(invocation) =>
        {
            Ok(())
        }
        SessionMetadataClaim::Existing(_) => Err(ToolInvokeError::Recoverable(format!(
            "session '{session_id}' was created by a competing invocation; the requested \
             metadata update conflicts"
        ))),
        // Orphaned and Vanished never escape the session constructor; the
        // match keeps this total if that ever changes.
        SessionMetadataClaim::Orphaned | SessionMetadataClaim::Vanished => {
            Err(ToolInvokeError::Recoverable(format!(
                "session '{session_id}' metadata creation race had no winner"
            )))
        }
    }
}
