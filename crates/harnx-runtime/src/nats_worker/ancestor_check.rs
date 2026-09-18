//! Refusing to resume a child whose parent invocation is over.
//!
//! A sub-agent session runs inside one tool call of its parent. If that
//! parent turn was interrupted — or already holds a result for the call —
//! nothing is waiting for the child's answer any more, so resuming it would
//! burn a model call whose output no one reads and whose tool cancel already
//! went out. The parent's own log is the authority for both.

use crate::nats_session_metadata::SessionMetadataStore;
use anyhow::{Context, Result};
use async_nats::jetstream::Context as JetstreamContext;
use harnx_core::session::SessionLogEntry;

/// How far up a parent chain the check walks before refusing to look further.
/// Sub-agent nesting is bounded well below this; a chain that is not is a
/// cycle or corruption, and failing is better than looping.
const MAX_ANCESTORS: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AncestorVerdict {
    Clear,
    Interrupted {
        parent_session: String,
        cancellation_id: Option<String>,
    },
}

/// Follow `SessionMetadata.parent` links upward (at most [`MAX_ANCESTORS`]
/// levels). A parent turn that issued this child's tool call and was
/// interrupted, or that already holds a result for the call, forbids resuming.
pub(super) async fn check_ancestors(
    jetstream: &JetstreamContext,
    metadata: &SessionMetadataStore,
    session_id: &str,
) -> Result<AncestorVerdict> {
    let mut current = session_id.to_string();
    for _ in 0..MAX_ANCESTORS {
        let Some(record) = metadata.get(&current).await? else {
            return Ok(AncestorVerdict::Clear);
        };
        let Some(link) = record.metadata.parent.clone() else {
            return Ok(AncestorVerdict::Clear);
        };
        let parent_log = crate::nats_session_log::NatsSessionLog::new(
            jetstream.clone(),
            link.session_id.clone(),
        );
        let entries = parent_log
            .load_events_latest_async()
            .await
            .with_context(|| format!("parent log unreadable: {}", link.session_id))?;
        if parent_call_is_closed(&entries, &link.tool_call_id) {
            return Ok(AncestorVerdict::Interrupted {
                parent_session: link.session_id,
                cancellation_id: latest_cancellation_id(&entries),
            });
        }
        current = link.session_id;
    }
    anyhow::bail!("parent chain deeper than {MAX_ANCESTORS} levels")
}

/// The call's `ToolCalls` is followed by a `Cancel` with no `ToolResults` for
/// the call in between, or a `ToolResults` already answers it.
fn parent_call_is_closed(entries: &[(u64, SessionLogEntry)], tool_call_id: &str) -> bool {
    let Some(call_pos) = entries.iter().position(|(_, entry)| {
        matches!(entry, SessionLogEntry::ToolCalls { calls, .. }
            if calls.iter().any(|call| call.id.as_deref() == Some(tool_call_id)))
    }) else {
        return false;
    };
    entries[call_pos + 1..]
        .iter()
        .any(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => results
                .iter()
                .any(|result| result.id.as_deref() == Some(tool_call_id)),
            SessionLogEntry::Cancel { .. } => true,
            _ => false,
        })
}

/// The newest cancellation id in the parent's log, so the child's own `Cancel`
/// can carry the same one through to the tools it stops.
fn latest_cancellation_id(entries: &[(u64, SessionLogEntry)]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Cancel {
                cancellation_id, ..
            } => Some(cancellation_id.clone()),
            _ => None,
        })
        .flatten()
}

#[cfg(test)]
#[path = "ancestor_check_tests.rs"]
mod tests;
