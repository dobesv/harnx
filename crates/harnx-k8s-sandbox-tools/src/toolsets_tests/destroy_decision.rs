//! Unit tests for the destroy decision on a claim whose delayed bind did
//! not apply: the re-read matrix and the bounded release, exercised through
//! the `destroy_untracked_with` seam with no NATS and no Kubernetes.

use super::*;
use crate::toolsets::binding::{destroy_untracked_with, UntrackedClaim};
use harnx_toolset::ToolInvokeError;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(30);

/// A release that records each attempt and resolves successfully.
fn release_recording(
    attempts: Arc<AtomicUsize>,
) -> impl FnOnce() -> std::future::Ready<anyhow::Result<()>> {
    move || {
        attempts.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(()))
    }
}

/// A hung release (like a delete stuck behind Kubernetes retry backoff):
/// each attempt is recorded and the future never resolves, so only the
/// deadline can end it.
fn release_hanging(
    attempts: Arc<AtomicUsize>,
) -> impl FnOnce() -> std::future::Pending<anyhow::Result<()>> {
    move || {
        attempts.fetch_add(1, Ordering::SeqCst);
        std::future::pending()
    }
}

/// The deadline, not a hung release, ends the destroy: the claim goes to
/// its TTL, the release was attempted exactly once, and the paused clock
/// jumping past the deadline proves this cannot hang the caller.
#[tokio::test(start_paused = true)]
async fn a_hung_release_times_out_and_leaves_the_claim_to_its_ttl() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let outcome = destroy_untracked_with(
        "claim-hung",
        Ok(None),
        release_hanging(attempts.clone()),
        DEADLINE,
    )
    .await;
    assert_eq!(outcome, UntrackedClaim::ReleaseTimedOut);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// A failed binding re-read is inconclusive: the release is never
/// attempted, so state is never destroyed on an uncertain check.
#[tokio::test]
async fn a_failed_binding_recheck_spares_the_claim() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let recheck = Err(ToolInvokeError::Recoverable(
        "metadata store timed out".into(),
    ));
    let outcome = destroy_untracked_with(
        "claim-recheck-err",
        recheck,
        release_recording(attempts.clone()),
        DEADLINE,
    )
    .await;
    assert_eq!(outcome, UntrackedClaim::Inconclusive);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

/// An absent binding is positive proof the claim is untracked: the release
/// destroys it.
#[tokio::test]
async fn an_absent_binding_proves_the_claim_untracked() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let outcome = destroy_untracked_with(
        "claim-absent",
        Ok(None),
        release_recording(attempts.clone()),
        DEADLINE,
    )
    .await;
    assert_eq!(outcome, UntrackedClaim::Destroyed);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// A binding that names another sandbox is positive proof the claim is
/// untracked: the release destroys it.
#[tokio::test]
async fn a_binding_naming_another_sandbox_proves_the_claim_untracked() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let outcome = destroy_untracked_with(
        "claim-replaced",
        Ok(Some(json!({"version": 1, "sandbox_id": "claim-other"}))),
        release_recording(attempts.clone()),
        DEADLINE,
    )
    .await;
    assert_eq!(outcome, UntrackedClaim::Destroyed);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// A binding that still names this claim spares it, even when unusable as
/// a binding.
#[tokio::test]
async fn a_binding_naming_the_claim_spares_it_even_when_unusable() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let outcome = destroy_untracked_with(
        "claim-named",
        Ok(Some(json!({"version": 2, "sandbox_id": "claim-named"}))),
        release_recording(attempts.clone()),
        DEADLINE,
    )
    .await;
    assert_eq!(outcome, UntrackedClaim::Spared);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

/// A stored value that cannot be interpreted as a binding at all is
/// inconclusive: the release is never attempted.
#[tokio::test]
async fn an_undecodable_binding_value_spares_the_claim() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let outcome = destroy_untracked_with(
        "claim-garbage",
        Ok(Some(json!(42))),
        release_recording(attempts.clone()),
        DEADLINE,
    )
    .await;
    assert_eq!(outcome, UntrackedClaim::Inconclusive);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

/// A release that fails outright leaves the claim to its TTL; the failure
/// is only logged, which the caller-facing tests assert at the flow level.
#[tokio::test]
async fn a_failing_release_leaves_the_claim_to_its_ttl() {
    let outcome = destroy_untracked_with(
        "claim-release-fail",
        Ok(None),
        || std::future::ready(Err::<(), _>(anyhow::anyhow!("scripted delete rejection"))),
        DEADLINE,
    )
    .await;
    assert_eq!(outcome, UntrackedClaim::ReleaseFailed);
}
