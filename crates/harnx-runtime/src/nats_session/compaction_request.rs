//! Manual compaction request: frontend submit primitive with tail guard.
//!
//! Mirrors `interrupt.rs` but adapted for compaction workflow:
//! - Uses `CompactRequest`/`CompactResult` entries instead of `Cancel`
//! - Tail guard checks for pending/recently-completed compaction
//! - Announces with `ControlCommand::Compact` + `SessionActivate` (no Interrupt signal)
//!
//! Tail guard logic (frontend fast-path):
//! - `CompactRequest` at tail → `AlreadyInFlight` (request pending)
//! - `CompactResult { Compacted | Unchanged }` at tail → `NothingToDo` (recently done)
//! - `CompactResult { Failed }` or other entry → proceed (allow retry)
//!
//! The worker is the authoritative backstop. On conflict, newer entries are brought back
//! and the decision re-evaluated. `compressing` flag serialization in the worker prevents
//! overlap with automatic compaction.

use super::NatsSession;
use crate::nats_session_log::{FencedAppend, NatsSessionLog};
use crate::nats_worker::{
    publish_control_command, publish_session_activate, publish_targeted_session_activate,
    ControlCommand, LocalWorkerTarget, SessionActivate, SessionActivationRoute,
};
use anyhow::{Context, Result};
use harnx_core::session::{CompactOutcome, SessionLogEntry};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long to wait for the append to complete before timing out.
const COMPACTION_APPEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum CAS retry attempts for tail conflict resolution.
const MAX_CAS_ATTEMPTS: usize = 16;

/// Result of submitting a compaction request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CompactSubmit {
    /// Compaction request was successfully submitted.
    Submitted { compaction_id: String },
    /// A compaction request is already in-flight (tail is unresolved CompactRequest).
    AlreadyInFlight { compaction_id: String },
    /// No compaction needed (tail is CompactResult with Compacted or Unchanged).
    NothingToDo { outcome: CompactOutcome },
}

/// Request parameters for a compaction submission.
pub struct CompactionRequest {
    /// Storage key of the session (`session_key(agent, id)`).
    pub session_id: String,
    /// Cluster key used for a cluster-shared activation route.
    pub cluster: String,
    pub replicas: usize,
    /// Unique ID for this compaction request (UUID v4).
    pub compaction_id: String,
    /// Who requested this compaction (optional).
    pub requested_by: Option<String>,
}

impl NatsSession {
    /// Request manual compaction of this session's transcript.
    ///
    /// This is the frontend entry point for triggering compaction on an idle
    /// or active session. It performs:
    /// 1. Tail fast-path guard: skip if compaction already pending or recently done
    /// 2. Durable append of `CompactRequest` entry (fence_token 0, lease-free)
    /// 3. Best-effort announcement: `ControlCommand::Compact` + `SessionActivate`
    ///
    /// Returns `CompactSubmit::Submitted` on success, or one of the skip variants
    /// if the tail indicates a pending or recent compaction.
    pub async fn request_compaction(&self, requested_by: Option<String>) -> Result<CompactSubmit> {
        let request = CompactionRequest {
            session_id: self.storage_key.clone(),
            cluster: self.config.cluster.clone(),
            replicas: self.attachment_replicas,
            compaction_id: uuid::Uuid::new_v4().to_string(),
            requested_by,
        };

        tokio::time::timeout(
            COMPACTION_APPEND_TIMEOUT,
            request_compaction_session(
                &self.jetstream,
                &self.client,
                &self.config.activation_route,
                request,
            ),
        )
        .await
        .context("timed out appending compaction request; retry")?
    }
}

/// Read the log's tail, decide whether to append, and append exactly one
/// `CompactRequest` entry with a fenced (expected-last-sequence) publish.
///
/// The tail guard is a best-effort fast-path:
/// - tail = `CompactRequest` (unresolved) → skip, return `AlreadyInFlight`
/// - tail = `CompactResult { Compacted | Unchanged }` → skip, return `NothingToDo`
/// - tail = `CompactResult { Failed }` OR any other entry (or empty) → proceed
///
/// A conflict brings back newer entries, and the decision is re-evaluated.
/// The worker (T4) is the authoritative dedupe backstop for racers.
pub async fn request_compaction_session(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    route: &SessionActivationRoute,
    request: CompactionRequest,
) -> Result<CompactSubmit> {
    let log = NatsSessionLog::new(js.clone(), request.session_id.clone());
    let mut entries: Vec<_> = log.last_entry_async().await?.into_iter().collect();

    for _ in 0..MAX_CAS_ATTEMPTS {
        // Check tail for fast-path decision
        if let Some(skip) = decide_compact_submit(&entries) {
            return Ok(skip);
        }

        // Proceed with append
        let tail = entries.last().map_or(0, |(seq, _)| *seq);
        let entry = SessionLogEntry::compact_request(
            request.compaction_id.clone(),
            request.requested_by.clone(),
        );

        match log
            .append_fenced(&entry, tail, &request.compaction_id)
            .await?
        {
            FencedAppend::Appended(request_seq) => {
                log::info!(
                    "nats session: compaction request appended session_id={} compaction_id={} request_seq={}",
                    request.session_id,
                    request.compaction_id,
                    request_seq
                );
                announce_compaction(
                    js,
                    client,
                    route,
                    AcceptedCompaction::new(&request, request_seq),
                );
                return Ok(CompactSubmit::Submitted {
                    compaction_id: request.compaction_id,
                });
            }
            FencedAppend::Conflict { entries: newer } => {
                // Our own lost ack shows up as a CompactRequest carrying our id.
                if let Some((_, SessionLogEntry::CompactRequest { compaction_id, .. })) =
                    newer.iter().find(|(_, e)| {
                        matches!(
                            e,
                            SessionLogEntry::CompactRequest { compaction_id: id, .. }
                            if *id == request.compaction_id
                        )
                    })
                {
                    // Already appended - treat as success
                    return Ok(CompactSubmit::Submitted {
                        compaction_id: compaction_id.clone(),
                    });
                }
                entries.extend(newer);
            }
        }
    }

    anyhow::bail!(
        "session log tail kept moving; compaction request not appended after {MAX_CAS_ATTEMPTS} attempts"
    )
}

/// Decide whether to skip the append based on the tail entry.
///
/// Returns `Some(CompactSubmit)` for skip cases, `None` to proceed.
pub(super) fn decide_compact_submit(entries: &[(u64, SessionLogEntry)]) -> Option<CompactSubmit> {
    let (_, tail) = entries.last()?;

    match tail {
        // Unresolved request still in flight - skip
        SessionLogEntry::CompactRequest { compaction_id, .. } => {
            Some(CompactSubmit::AlreadyInFlight {
                compaction_id: compaction_id.clone(),
            })
        }
        // Compacted or Unchanged result - nothing new to do
        SessionLogEntry::CompactResult {
            outcome: outcome @ CompactOutcome::Compacted,
            ..
        }
        | SessionLogEntry::CompactResult {
            outcome: outcome @ CompactOutcome::Unchanged(_),
            ..
        } => Some(CompactSubmit::NothingToDo {
            outcome: outcome.clone(),
        }),
        // Failed result or any other entry (including no entries) - proceed
        SessionLogEntry::CompactResult {
            outcome: CompactOutcome::Failed(_),
            ..
        }
        | SessionLogEntry::Message { .. }
        | SessionLogEntry::ToolCalls { .. }
        | SessionLogEntry::ToolResults { .. }
        | SessionLogEntry::TurnEnd { .. }
        | SessionLogEntry::Cancel { .. }
        | SessionLogEntry::Error { .. }
        | SessionLogEntry::Compress { .. }
        | SessionLogEntry::SubAgentStarted { .. }
        | SessionLogEntry::HandoffCommitted { .. }
        | SessionLogEntry::HitlApprovalRequested { .. }
        | SessionLogEntry::HitlApprovalDecision { .. }
        | SessionLogEntry::DataUrls { .. }
        | SessionLogEntry::Clear
        | SessionLogEntry::EditEntries { .. }
        | SessionLogEntry::Rewind { .. }
        | SessionLogEntry::Unknown => None,
    }
}

/// The pieces of an accepted compaction request that `announce_compaction` needs.
struct AcceptedCompaction {
    session_id: String,
    cluster: String,
    replicas: usize,
    compaction_id: String,
    request_seq: u64,
}

impl AcceptedCompaction {
    fn new(request: &CompactionRequest, request_seq: u64) -> Self {
        Self {
            session_id: request.session_id.clone(),
            cluster: request.cluster.clone(),
            replicas: request.replicas,
            compaction_id: request.compaction_id.clone(),
            request_seq,
        }
    }
}

/// Start the best-effort wake-ups an accepted compaction request owes:
/// a `ControlCommand::Compact` hint for a live worker and a `SessionActivate`
/// so a dead worker's session still gets processed.
///
/// Runs on a spawned task because acceptance is the append and nothing else.
/// Awaiting JetStream activation here would put it inside the caller's budget.
fn announce_compaction(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    route: &SessionActivationRoute,
    accepted: AcceptedCompaction,
) {
    let js = js.clone();
    let client = client.clone();
    let route = route.clone();
    tokio::spawn(async move {
        let hint = ControlCommand::Compact {
            compaction_id: accepted.compaction_id.clone(),
        };
        if let Err(error) = publish_control_command(&client, &accepted.session_id, &hint).await {
            log::debug!("compact hint not published: {error:#}");
        }

        let result = match &route {
            SessionActivationRoute::ClusterShared => {
                let activation = SessionActivate::new(&accepted.session_id)
                    .with_requested_seq(accepted.request_seq);
                publish_session_activate(&js, &accepted.cluster, &activation, accepted.replicas)
                    .await
            }
            SessionActivationRoute::WorkerTargeted {
                session_scope,
                worker_id,
            } => {
                let activation = SessionActivate::targeted(
                    &accepted.session_id,
                    accepted.request_seq,
                    worker_id,
                );
                match LocalWorkerTarget::new(session_scope, worker_id) {
                    Ok(target) => publish_targeted_session_activate(&js, target, &activation).await,
                    Err(error) => Err(error),
                }
            }
        };

        if let Err(error) = result {
            log::debug!("compact activation not published: {error:#}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::session::UnchangedReason;

    #[test]
    fn decide_empty_entries_returns_none() {
        let entries: Vec<(u64, SessionLogEntry)> = vec![];
        assert!(decide_compact_submit(&entries).is_none());
    }

    #[test]
    fn decide_pending_request_returns_already_in_flight() {
        let entries = vec![(
            1u64,
            SessionLogEntry::compact_request("comp-1", Some("test".into())),
        )];
        let result = decide_compact_submit(&entries);
        assert_eq!(
            result,
            Some(CompactSubmit::AlreadyInFlight {
                compaction_id: "comp-1".into()
            })
        );
    }

    #[test]
    fn decide_compacted_result_returns_nothing_to_do() {
        let entries = vec![(
            2u64,
            SessionLogEntry::compact_result("comp-1", CompactOutcome::Compacted),
        )];
        let result = decide_compact_submit(&entries);
        assert_eq!(
            result,
            Some(CompactSubmit::NothingToDo {
                outcome: CompactOutcome::Compacted
            })
        );
    }

    #[test]
    fn decide_unchanged_result_returns_nothing_to_do() {
        let entries = vec![(
            2u64,
            SessionLogEntry::compact_result(
                "comp-1",
                CompactOutcome::Unchanged(UnchangedReason::NoUserMessages),
            ),
        )];
        let result = decide_compact_submit(&entries);
        assert_eq!(
            result,
            Some(CompactSubmit::NothingToDo {
                outcome: CompactOutcome::Unchanged(UnchangedReason::NoUserMessages)
            })
        );
    }

    #[test]
    fn decide_failed_result_returns_none() {
        let entries = vec![(
            2u64,
            SessionLogEntry::compact_result("comp-1", CompactOutcome::Failed("error".into())),
        )];
        assert!(decide_compact_submit(&entries).is_none());
    }

    #[test]
    fn decide_user_message_returns_none() {
        use harnx_core::message::{MessageContent, MessageRole};
        let entries = vec![(
            1u64,
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("hello".into()),
                timestamp: None,
                fence_token: None,
            },
        )];
        assert!(decide_compact_submit(&entries).is_none());
    }
}
