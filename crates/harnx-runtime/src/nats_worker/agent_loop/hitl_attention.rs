//! HITL approval request handling and attention bumping.
//!
//! This module contains the callback builders and handlers for appending
//! HITL (Human-in-the-Loop) approval requests to the session log and
//! bumping session attention state.

use super::super::backend::{FencedSessionLogSink, NatsSessionLogBackend};
use crate::nats_event_sink::NatsEventSink;
use crate::nats_lease::NatsSessionLease;
use crate::nats_session_metadata::SessionMetadataStore;
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::session::SessionLogEntry;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// Context for building HITL approval callbacks.
///
/// Bundles the cohesive set of worker/session/store handles needed to
/// construct a HITL approval callback, reducing argument count and
/// representing a clear domain concept.
pub(super) struct HitlCallbackContext<'a> {
    pub jetstream: &'a jetstream::Context,
    pub session_id: &'a str,
    pub lease: &'a Arc<NatsSessionLease>,
    pub event_sink: Option<&'a Arc<NatsEventSink>>,
    pub after_seq_observer: Option<&'a Arc<AtomicU64>>,
    pub metadata_store: Option<&'a SessionMetadataStore>,
}

/// Runtime state for HITL approval request handling.
///
/// Built from `HitlCallbackContext` after constructing backend/sink,
/// this bundles the handles needed during the approval request flow.
pub(super) struct HitlApprovalState {
    pub backend: NatsSessionLogBackend,
    pub sink: FencedSessionLogSink,
    pub lease: Arc<NatsSessionLease>,
    pub event_sink: Option<Arc<NatsEventSink>>,
}

impl<'a> HitlCallbackContext<'a> {
    /// Build the backend and sink for this context.
    fn build_backend_and_sink(&self) -> (NatsSessionLogBackend, FencedSessionLogSink) {
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), self.session_id)
            .with_after_seq_observer(
                self.after_seq_observer
                    .cloned()
                    .unwrap_or_else(|| Arc::new(AtomicU64::new(0))),
            )
            .with_metadata_store(self.metadata_store.cloned());
        let sink = FencedSessionLogSink::new(backend.clone(), Arc::clone(self.lease));
        (backend, sink)
    }

    /// Build the HITL approval callback.
    fn build_callback(self) -> crate::agent_loop::OnHitlApprovalRequiredFn {
        let state = self.build_state();
        Arc::new(move |deferred| handle_hitl_approval_request(deferred, &state))
    }

    /// Build the runtime state for HITL approval handling.
    fn build_state(self) -> HitlApprovalState {
        let (backend, sink) = self.build_backend_and_sink();
        HitlApprovalState {
            backend,
            sink,
            lease: Arc::clone(self.lease),
            event_sink: self.event_sink.cloned(),
        }
    }
}

/// Build the HITL approval request callback for tests.
#[cfg(test)]
pub(crate) fn build_hitl_approval_request_callback_for_test(
    ctx: HitlCallbackContext<'_>,
) -> crate::agent_loop::OnHitlApprovalRequiredFn {
    ctx.build_callback()
}

/// Build the HITL approval request callback.
pub(super) fn build_hitl_approval_request_callback(
    ctx: HitlCallbackContext<'_>,
) -> crate::agent_loop::OnHitlApprovalRequiredFn {
    ctx.build_callback()
}

/// Handle a HITL approval request by appending to the log and bumping attention.
///
/// Returns the tool call ID of the pending approval (either existing or newly created).
pub(super) fn handle_hitl_approval_request(
    deferred: &crate::tool::DeferredToolCall,
    state: &HitlApprovalState,
) -> Result<String> {
    anyhow::ensure!(
        state.lease.is_held(),
        "session lease lost before HITL approval request"
    );

    let tool_call_id = deferred
        .call
        .id
        .clone()
        .context("deferred tool call has no tool_call_id")?;

    let entry = build_hitl_entry(deferred, &state.lease);

    match try_append_with_retries(state, &entry, &tool_call_id)? {
        HitlOutcome::ExistingPending(id) => Ok(id),
        HitlOutcome::Appended(id) => Ok(id),
        HitlOutcome::Fallback(id) => Ok(id),
    }
}

/// Build the HITL log entry from a deferred tool call.
fn build_hitl_entry(
    deferred: &crate::tool::DeferredToolCall,
    lease: &NatsSessionLease,
) -> SessionLogEntry {
    let tool_call_id = deferred.call.id.clone().unwrap_or_default();
    let summary = deferred
        .reason
        .clone()
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| format!("Approve tool call `{}`", deferred.call.name));
    SessionLogEntry::HitlApprovalRequested {
        tool_call_id,
        summary,
        fence_token: lease.fence_token(),
    }
}

/// Outcome of HITL approval handling.
enum HitlOutcome {
    ExistingPending(String),
    Appended(String),
    Fallback(String),
}

/// Try to append HITL approval with retries, returning the outcome.
fn try_append_with_retries(
    state: &HitlApprovalState,
    entry: &SessionLogEntry,
    tool_call_id: &str,
) -> Result<HitlOutcome> {
    for attempt in 0..3 {
        match try_append_hitl_approval(state, entry) {
            Ok(AppendHitlResult::ExistingPending(pending_id)) => {
                log::info!(
                    "HITL approval request: using existing pending approval {} (attempt {})",
                    pending_id,
                    attempt
                );
                notify_session_updated(&state.event_sink);
                return Ok(HitlOutcome::ExistingPending(pending_id));
            }
            Ok(AppendHitlResult::Appended { assigned_seq }) => {
                log::info!(
                    "HITL approval request: appended at seq {} (attempt {})",
                    assigned_seq,
                    attempt
                );
                bump_attention_after_hitl_append(&state.backend, assigned_seq);
                notify_session_updated(&state.event_sink);
                return Ok(HitlOutcome::Appended(tool_call_id.to_string()));
            }
            Err(err) if err.to_string().contains("CAS race") => {
                log::debug!("HITL approval CAS race on attempt {}, retrying", attempt);
                continue;
            }
            Err(err) => {
                return Err(err);
            }
        }
    }

    // Final attempt: derive status from log
    let entries = state.backend.load_events_blocking()?;
    let pending = super::derive_pending_hitl_approvals(&entries)?;
    if let Some(oldest) = pending.first() {
        let pending_id = oldest.tool_call_id.clone();
        log::warn!(
            "HITL approval request: falling back to existing pending {} after exhausting retries",
            pending_id
        );
        bump_attention_after_hitl_append(&state.backend, oldest.seq);
        notify_session_updated(&state.event_sink);
        return Ok(HitlOutcome::Fallback(pending_id));
    }

    anyhow::bail!("HITL approval request lost repeated concurrent append races")
}

/// Result of attempting to append a HITL approval request.
pub(super) enum AppendHitlResult {
    /// An existing pending approval was found; return its tool call ID.
    ExistingPending(String),
    /// The approval was successfully appended at the given sequence.
    Appended { assigned_seq: u64 },
}

/// Attempt to append a HITL approval request, returning early if one is already pending.
fn try_append_hitl_approval(
    state: &HitlApprovalState,
    entry: &SessionLogEntry,
) -> Result<AppendHitlResult> {
    anyhow::ensure!(
        state.lease.is_held(),
        "session lease lost before HITL approval request"
    );

    let entries = state.backend.load_events_blocking()?;
    let pending = super::derive_pending_hitl_approvals(&entries)?;

    if let Some(oldest) = pending.first() {
        log::warn!(
            "session already has pending HITL approval; retaining oldest request: tool_call_id={} seq={}",
            oldest.tool_call_id,
            oldest.seq
        );
        return Ok(AppendHitlResult::ExistingPending(
            oldest.tool_call_id.clone(),
        ));
    }

    let expected_last_sequence = entries.last().map_or(0, |(seq, _)| *seq);

    match state
        .sink
        .append_hitl_event_cas_blocking(entry, expected_last_sequence)?
    {
        Some(assigned_seq) => Ok(AppendHitlResult::Appended { assigned_seq }),
        None => Err(anyhow::anyhow!("CAS race, retry")),
    }
}

/// Bump attention after a successful HITL approval append.
fn bump_attention_after_hitl_append(backend: &NatsSessionLogBackend, assigned_seq: u64) {
    if let Some(store) = backend.metadata_store_opt() {
        if let Err(error) = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(store.bump_attention(backend.session_id(), assigned_seq))
        }) {
            log::warn!(
                "failed to bump attention after HITL approval request: session_id={} seq={} error={error:#}",
                backend.session_id(),
                assigned_seq
            );
        }
    }
}

/// Notify listeners that the session state has changed.
fn notify_session_updated(event_sink: &Option<Arc<NatsEventSink>>) {
    if let Some(event_sink) = event_sink {
        event_sink.publish_session_updated();
    }
}

#[cfg(test)]
mod hitl_attention_tests {
    use super::*;
    use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
    use crate::tool::DeferredToolCall;
    use harnx_core::require_nextest;
    use harnx_core::tool::ToolCall;

    /// Test that `build_hitl_approval_request_callback_for_test` appends
    /// `HitlApprovalRequested` and bumps attention directly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hitl_approval_callback_bumps_attention_directly() {
        require_nextest();
        let Some((url, mut child, _store_dir)) = crate::nats_worker::tests::spawn_test_nats().await
        else {
            return;
        };

        let client = async_nats::connect(&url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);
        let store = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
        let session_id = crate::nats_worker::new_remote_session_id();
        let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);

        // Create session metadata
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await
            .unwrap();

        // Acquire a lease for the session
        let lease =
            crate::nats_worker::backend::test_session_authority(&jetstream, &storage_key, &store)
                .await;
        let after_seq_observer = Arc::new(AtomicU64::new(0));

        // Build the callback with metadata store attached
        let ctx = HitlCallbackContext {
            jetstream: &jetstream,
            session_id: &storage_key,
            lease: &lease,
            event_sink: None,
            after_seq_observer: Some(&after_seq_observer),
            metadata_store: Some(&store),
        };
        let callback = build_hitl_approval_request_callback_for_test(ctx);

        // Invoke the callback with a deferred tool call
        let deferred = DeferredToolCall {
            call: ToolCall::new(
                "test_tool".to_string(),
                serde_json::json!({"arg": "value"}),
                Some("call-123".to_string()),
                None,
            ),
            arguments: serde_json::json!({"arg": "value"}),
            reason: Some("Test approval".to_string()),
        };
        let result = callback(&deferred).unwrap();
        assert_eq!(result, "call-123");

        // Verify session is now unread with correct attention seq
        let state = store.get_read_state(&storage_key).await.unwrap();
        assert!(
            state.is_unread(),
            "session should be unread after HITL approval request callback"
        );
        assert!(
            state.last_attention_seq >= 1,
            "last_attention_seq should be at least 1"
        );

        lease.release().await.unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
