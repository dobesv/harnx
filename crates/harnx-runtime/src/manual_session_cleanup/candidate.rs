//! One candidate's decision inside the manual cleanup pass: the read-only
//! guards, the preview, and the guarded deletion cascade. Split from the
//! module root so the pass loop (lease ownership, bounds) and the
//! per-candidate rules stay separately readable.
use super::report::{CandidateRun, CleanupCandidate, PassAdmission};
use super::{
    ManualCleanupOptions, ManualCleanupOverrides, SessionCleanupResult, SessionCleanupStatus,
};
use crate::nats_admin::a2a;
use crate::nats_admin::{CanonicalMetadataPurge, GuardedSessionDeletion, SessionDeleteResult};
use crate::nats_lease::NatsSessionLease;
use crate::nats_session_metadata::{metadata_key, SessionMetadataStore};
use crate::remote_session_cleanup::{
    a2a_lease_present, session_lease_present, session_reactivated,
};
use anyhow::{Context as _, Result};
use async_nats::jetstream::kv::Operation;

/// Handles and settings every candidate decision shares, plus the GC lease
/// whose live state guards each admission. The pass loop in the module root
/// builds one of these per run; the field set is the loop's open handles.
pub(super) struct CandidateProcessor<'a> {
    pub(super) jetstream: &'a async_nats::jetstream::Context,
    pub(super) metadata_store: &'a SessionMetadataStore,
    pub(super) lease_store: Option<&'a async_nats::jetstream::kv::Store>,
    pub(super) threshold: u64,
    pub(super) options: &'a ManualCleanupOptions,
    pub(super) overrides: &'a ManualCleanupOverrides,
    pub(super) lease: &'a NatsSessionLease,
}

impl CandidateProcessor<'_> {
    /// Drive the candidate loop to its end — natural exhaustion, the
    /// admission bound, or GC-lease loss — then revalidate ownership once
    /// more before the caller reports. The loop's own revalidation never
    /// runs after the final or max-capped candidate (the bound check breaks
    /// first), and a broker-side loss there only flips the local watch at
    /// the next renewal tick, so this trailing revalidation is what closes
    /// the window; `release()` cannot, because it treats a lost or
    /// reclaimed lease as a successful no-op.
    pub(super) async fn run_pass(
        &self,
        cluster: &str,
        candidates: Vec<CleanupCandidate>,
        run: &mut CandidateRun,
    ) {
        for candidate in candidates {
            if run.lease_lost {
                break;
            }
            if run.admissions(self.options) >= self.options.max {
                run.bound_reached = true;
                break;
            }
            // Renewal-task loss or a broker-authoritative revalidation
            // failure both stop new admissions; an already-running cascade
            // finishes under its own internal guards.
            if !super::gc_lease_live(self.lease).await {
                run.lease_lost = true;
                log::warn!(
                    "manual expired-session cleanup lost the GC lease; stopping admissions: cluster={cluster}"
                );
                break;
            }
            if let Some(hook) = &self.overrides.before_candidate {
                hook(&candidate.storage_key).await;
            }
            let result = self.decide(run, candidate).await;
            run.results.push(result);
        }
        if !super::gc_lease_live(self.lease).await {
            run.lease_lost = true;
            log::warn!(
                "manual expired-session cleanup lost the GC lease before finishing: cluster={cluster}"
            );
        }
    }

    /// One candidate's full decision: read-only guards, then either the
    /// preview or the guarded deletion cascade. Infallible by design — a
    /// per-candidate failure (guard read error, stream-probe error) is
    /// recorded as that candidate's result so one bad session can never
    /// abort the pass and discard the work already reported.
    pub(super) async fn decide(
        &self,
        run: &mut CandidateRun,
        candidate: CleanupCandidate,
    ) -> SessionCleanupResult {
        match self.pre_admission(&candidate).await {
            Ok(PassAdmission::Active(detail)) => skip_active(&candidate, false, detail),
            Ok(PassAdmission::RevisionChanged { missing }) => {
                revision_changed(&candidate, missing, false, false)
            }
            Ok(PassAdmission::Ready) if self.options.dry_run => {
                match session_stream_bytes(self.jetstream, &candidate.storage_key, self.overrides)
                    .await
                {
                    Ok(stream_bytes) => {
                        let mut preview = SessionCleanupResult::new(
                            candidate.storage_key,
                            SessionCleanupStatus::WouldDelete,
                        );
                        preview.stream_bytes = stream_bytes;
                        preview
                    }
                    // Pre-mutation, no admission: a per-session error that
                    // still lets the preview cover the remaining candidates.
                    Err(error) => stream_probe_error(&candidate.storage_key, &error),
                }
            }
            Ok(PassAdmission::Ready) => self.guarded_deletion(run, candidate).await,
            // A guard the pass must inspect failed (transport or malformed
            // A2A state): a per-session error, never a deletion and never a
            // false no-hold.
            Err(error) => {
                log::warn!(
                    "manual expired-session cleanup guard failed: session_id={} err={error:#}",
                    candidate.storage_key
                );
                let mut error_result =
                    SessionCleanupResult::new(candidate.storage_key, SessionCleanupStatus::Error);
                error_result.detail = SessionCleanupResult::error_detail(&error);
                error_result
            }
        }
    }

    /// The guarded deletion cascade for one admitted candidate. The attempt
    /// is charged before the cascade entry (an A2A fence refusal consumes
    /// it); a cascade-entry revision change refunds it because nothing was
    /// mutated. A stream-probe failure here is pre-mutation and pre-charge:
    /// an error result, not a deletion attempt.
    async fn guarded_deletion(
        &self,
        run: &mut CandidateRun,
        candidate: CleanupCandidate,
    ) -> SessionCleanupResult {
        let storage_key = candidate.storage_key.clone();
        let stream_bytes =
            match session_stream_bytes(self.jetstream, &storage_key, self.overrides).await {
                Ok(stream_bytes) => stream_bytes,
                Err(error) => return stream_probe_error(&storage_key, &error),
            };
        run.attempted += 1;
        if let Some(hook) = &self.overrides.after_admission {
            hook(&storage_key).await;
        }
        let result = match GuardedSessionDeletion::begin(
            self.jetstream,
            &storage_key,
            candidate.metadata_revision,
        )
        .await
        {
            Ok(GuardedSessionDeletion::RevisionChanged { missing }) => {
                run.attempted -= 1;
                revision_changed(&candidate, missing, false, false)
            }
            Ok(started) => {
                if let Some(hook) = &self.overrides.after_entry {
                    hook(&storage_key).await;
                }
                let completed = started
                    .finish(self.jetstream, self.lease_store.cloned(), &storage_key)
                    .await;
                run.completed_cascades += 1;
                cascade_outcome(completed, &candidate, stream_bytes)
            }
            Err(error) if fence_refused(&error) => {
                log::warn!(
                    "manual expired-session cleanup: deletion fence refused: session_id={storage_key} err={error:#}"
                );
                skip_active(&candidate, true, "A2A deletion fence refused the race")
            }
            Err(error) => {
                log::warn!(
                    "manual expired-session cleanup cascade entry failed: session_id={storage_key} err={error:#}"
                );
                let mut failed =
                    SessionCleanupResult::new(storage_key, SessionCleanupStatus::Failed);
                failed.stream_bytes = stream_bytes;
                failed.detail = SessionCleanupResult::error_detail(&error);
                failed
            }
        };
        // Check loss again after the cascade, before the next admission.
        if !gc_lease_current(self.lease) {
            run.lease_lost = true;
        }
        if self.overrides.simulate_lease_loss_after == Some(run.completed_cascades) {
            self.lease.mark_lost_for_test();
            run.lease_lost = true;
        }
        result
    }

    /// Read-only guards, mirroring the deletion fence's own decision inputs
    /// through the shared leader-read A2A predicate. Every check re-reads
    /// from the leader; nothing here mutates session-owned state.
    async fn pre_admission(&self, candidate: &CleanupCandidate) -> Result<PassAdmission> {
        let key = metadata_key(&candidate.storage_key);
        let entry = self
            .metadata_store
            .leader_entry(&key)
            .await
            .with_context(|| format!("Failed to inspect session metadata key '{key}'"))?;
        let Some(entry) = entry.filter(|entry| entry.operation == Operation::Put) else {
            return Ok(PassAdmission::RevisionChanged { missing: true });
        };
        if entry.revision != candidate.metadata_revision {
            return Ok(PassAdmission::RevisionChanged { missing: false });
        }
        if session_reactivated(self.metadata_store, &candidate.storage_key, self.threshold).await? {
            return Ok(PassAdmission::Active("fresh activity since the snapshot"));
        }
        if session_lease_present(self.lease_store, &candidate.storage_key).await? {
            return Ok(PassAdmission::Active("session lease is live"));
        }
        if a2a_lease_present(self.lease_store, &candidate.storage_key).await? {
            return Ok(PassAdmission::Active("A2A lease is live"));
        }
        let holds = a2a::leader_read_a2a_holds(self.metadata_store, &candidate.storage_key).await?;
        if holds.unresolved_context {
            return Ok(PassAdmission::Active("A2A context has unresolved work"));
        }
        if holds.unsettled_first_reservation {
            return Ok(PassAdmission::Active(
                "A2A first-message reservation is unsettled",
            ));
        }
        Ok(PassAdmission::Ready)
    }
}

/// Interpret the completed cascade: a purged (or already-gone) canonical
/// record is a deletion; a late CAS rejection or a cascade error means
/// effects may already exist, reported honestly rather than as a no-op.
fn cascade_outcome(
    completed: Result<SessionDeleteResult>,
    candidate: &CleanupCandidate,
    stream_bytes: u64,
) -> SessionCleanupResult {
    match completed {
        Ok(deleted) => match deleted.canonical_metadata {
            CanonicalMetadataPurge::Purged | CanonicalMetadataPurge::Absent => {
                let mut deleted = SessionCleanupResult::new(
                    candidate.storage_key.clone(),
                    SessionCleanupStatus::Deleted,
                );
                deleted.stream_bytes = stream_bytes;
                deleted
            }
            CanonicalMetadataPurge::RevisionChanged => {
                revision_changed(candidate, false, true, true)
            }
        },
        Err(error) => {
            log::warn!(
                "manual expired-session cleanup cascade failed: session_id={} err={error:#}",
                candidate.storage_key
            );
            let mut failed = SessionCleanupResult::new(
                candidate.storage_key.clone(),
                SessionCleanupStatus::Failed,
            );
            failed.stream_bytes = stream_bytes;
            failed.partial_possible = true;
            failed.detail = SessionCleanupResult::error_detail(&error);
            failed
        }
    }
}

/// Post-cascade loss check without a broker round trip; the next admission
/// revalidates authoritatively.
fn gc_lease_current(lease: &NatsSessionLease) -> bool {
    *lease.lost_watch().borrow() && lease.is_held()
}

/// Whether the cascade entry failed with a live A2A fence refusal rather
/// than a transport or malformed-state failure.
// qual:allow(coupling, deh) reason: "FenceRefused is a typed refusal carried inside the cascade's anyhow error chain; downcast at the pass boundary is the intended classification, not a design leak"
fn fence_refused(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<a2a::FenceRefused>().is_some())
}

fn skip_active(candidate: &CleanupCandidate, late: bool, detail: &str) -> SessionCleanupResult {
    let mut result = SessionCleanupResult::new(
        candidate.storage_key.clone(),
        SessionCleanupStatus::SkippedActive,
    );
    result.late = late;
    result.detail = Some(detail.to_string());
    result
}

fn revision_changed(
    candidate: &CleanupCandidate,
    missing: bool,
    late: bool,
    partial_possible: bool,
) -> SessionCleanupResult {
    let mut result = SessionCleanupResult::new(
        candidate.storage_key.clone(),
        SessionCleanupStatus::SkippedRevisionChanged,
    );
    result.late = late;
    result.missing_metadata = missing;
    result.partial_possible = partial_possible;
    result
}

/// A per-session error from the pre-mutation stream probe: recorded, never
/// fatal, and (in real runs) never charged as an admission.
fn stream_probe_error(storage_key: &str, error: &anyhow::Error) -> SessionCleanupResult {
    log::warn!(
        "manual expired-session cleanup: session stream inspection failed: session_id={storage_key} err={error:#}"
    );
    let mut result =
        SessionCleanupResult::new(storage_key.to_string(), SessionCleanupStatus::Error);
    result.detail = SessionCleanupResult::error_detail(error);
    result
}

/// Pre-delete session-stream bytes; a metadata-only session (no stream)
/// contributes zero, never an error. The `fail_stream_probe_for` test hook
/// injects the broker-side stream-info failure nothing in this cluster can
/// produce on demand.
async fn session_stream_bytes(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
    overrides: &ManualCleanupOverrides,
) -> Result<u64> {
    if overrides.fail_stream_probe_for.as_deref() == Some(storage_key) {
        anyhow::bail!("injected stream probe failure for '{storage_key}'");
    }
    let name = crate::nats_session_log::stream_name_for_session(storage_key);
    match jetstream.get_stream(&name).await {
        Ok(stream) => Ok(stream.cached_info().state.bytes),
        Err(error) if crate::nats_admin::stream_missing(&error.kind()) => Ok(0),
        Err(error) => Err(error).with_context(|| {
            format!("Failed to inspect session stream '{name}' for '{storage_key}'")
        }),
    }
}
