//! The first-writer checkpoint guards.

use harnx_toolset::{ToolInvocationContext, ToolInvokeError};
use serde_json::{json, Value};

/// Checkpoint field pinning an explicit-ID creation guard.
const CREATION_GUARD_FIELD: &str = "metadata_creation";
/// Checkpoint field pinning the metadata revision an existing-session patch
/// must CAS against.
const REVISION_GUARD_FIELD: &str = "metadata_revision";

/// The guard a first writer pins in the invocation checkpoint before any
/// mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MetadataGuard {
    Creation,
    Revision(u64),
}

pub(super) fn guard_of(checkpoint: Option<&Value>) -> Option<MetadataGuard> {
    let checkpoint = checkpoint?;
    if checkpoint[CREATION_GUARD_FIELD].as_bool() == Some(true) {
        return Some(MetadataGuard::Creation);
    }
    checkpoint[REVISION_GUARD_FIELD]
        .as_u64()
        .map(MetadataGuard::Revision)
}

pub(super) fn insert_guard(value: &mut Value, guard: MetadataGuard) {
    match guard {
        MetadataGuard::Creation => {
            value[CREATION_GUARD_FIELD] = json!(true);
        }
        MetadataGuard::Revision(revision) => {
            value[REVISION_GUARD_FIELD] = json!(revision);
        }
    }
}

/// Write the guard checkpoint through the caller's checkpoint store (the
/// same first-writer store the tools already checkpoint through), returning
/// the stored guard. The checkpoint winner's pin always wins; the guard is
/// never silently rebased. The winner must also carry this attempt's trusted
/// scope: a first writer pinned under a different route (a same-broker
/// cluster-label change between attempts) is a stale scope, not a valid guard
/// here (review F03). Guard fields themselves stay additive; only the pin
/// comparison below decides between them.
pub(super) async fn pin_guard(
    context: &ToolInvocationContext,
    proposed: Value,
    guard: MetadataGuard,
    session_id: &str,
) -> Result<MetadataGuard, ToolInvokeError> {
    let Some(store) = context.checkpoint_store.as_ref() else {
        return Err(ToolInvokeError::Recoverable(
            "child metadata update requires a durable invocation checkpoint".into(),
        ));
    };
    let stored = store.checkpoint(proposed.clone()).await.map_err(|error| {
        ToolInvokeError::Fatal(format!("persist sub-agent metadata checkpoint: {error:#}"))
    })?;
    // Every non-guard scope field the caller proposed must match the winner:
    // session id, storage key, cluster and the async dispatch's prompt and
    // parent keys.
    let same_scope = proposed
        .as_object()
        .zip(stored.as_object())
        .is_some_and(|(proposed, _)| {
            proposed
                .iter()
                .filter(|(key, _)| !is_guard_field(key))
                .all(|(key, value)| stored.get(key) == Some(value))
        });
    if !same_scope {
        return Err(ToolInvokeError::Fatal(
            "sub-agent metadata checkpoint scope mismatch".into(),
        ));
    }
    if stored["session_id"].as_str() != Some(session_id) {
        return Err(ToolInvokeError::Fatal(
            "sub-agent metadata checkpoint names a different session".into(),
        ));
    }
    match (guard, guard_of(Some(&stored))) {
        // The first writer predates the guard fields (a legacy checkpoint):
        // this proposal pins, same as before.
        (proposed, None) => Ok(proposed),
        // Same kind: the winner's pin wins. A Revision proposal honors a
        // stored Revision regardless of numeric equality — sibling attempts
        // can have observed different revisions — and the stored number is
        // authoritative for the CAS.
        (MetadataGuard::Creation, Some(MetadataGuard::Creation)) => Ok(MetadataGuard::Creation),
        (MetadataGuard::Revision(_), Some(stored @ MetadataGuard::Revision(_))) => Ok(stored),
        // Incompatible kinds never rebase across decisions made from
        // different observations (review F04): a stale Creation proposal
        // must not recreate a child deleted after a sibling pinned the
        // existing revision, and a stale Revision proposal must not ride a
        // pinned creation guard.
        (MetadataGuard::Creation, Some(MetadataGuard::Revision(_)))
        | (MetadataGuard::Revision(_), Some(MetadataGuard::Creation)) => {
            Err(ToolInvokeError::Fatal(
                "sub-agent metadata checkpoint guard disagrees with the pinned kind".into(),
            ))
        }
    }
}

/// The additive first-writer guard fields, excluded from the scope
/// comparison above: a stored winner's different pin is adjudicated by
/// `guard_of`, never by scope.
fn is_guard_field(key: &str) -> bool {
    key == CREATION_GUARD_FIELD || key == REVISION_GUARD_FIELD
}

/// The call id every guard and mutation attributes this invocation by.
pub(super) fn stable_invocation(context: &ToolInvocationContext) -> Result<&str, ToolInvokeError> {
    let call_id = context.call_id.as_str();
    if call_id.trim().is_empty() {
        return Err(ToolInvokeError::Fatal(
            "child metadata update requires stable invocation identity".into(),
        ));
    }
    Ok(call_id)
}
