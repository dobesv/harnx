//! Writing and clearing the session's ambient sandbox binding.

use super::lifecycle_toolset::LifecycleToolset;
use super::{lifecycle_error, recoverable, SandboxBinding, SANDBOX_CONTEXT_KEY};
use crate::lifecycle::is_sandbox_not_found;
use harnx_runtime::nats_session_metadata::ToolContextEntry;
use harnx_toolset::{ToolInvocationContext, ToolInvokeError};
use serde_json::Value;
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Ceiling on untracked-claim cleanup: destroying must not hold the
/// activation error hostage behind Kubernetes retry backoff (which can
/// run to minutes with server `Retry-After`). Past the deadline the claim
/// is left to its TTL.
const DESTROY_DEADLINE: Duration = Duration::from_secs(30);

/// Whether the raw stored binding names this sandbox, even when unusable as
/// a binding (unknown version, blank ID): a value that still points at the
/// claim means the session tracks it, and destroying the claim would take a
/// live sandbox with it. A well-formed binding naming another sandbox
/// proves the claim untracked; a value that cannot be interpreted as a
/// binding at all is inconclusive, and an inconclusive check must not
/// destroy state.
enum TracksClaim {
    Yes,
    No,
    Inconclusive,
}

fn classify_stored_binding(value: &Value, sandbox_id: &str) -> TracksClaim {
    match serde_json::from_value::<SandboxBinding>(value.clone()) {
        Ok(binding) if binding.sandbox_id == sandbox_id => TracksClaim::Yes,
        Ok(_) => TracksClaim::No,
        Err(_) => TracksClaim::Inconclusive,
    }
}

/// What happened to a claim whose delayed bind did not apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UntrackedClaim {
    /// The re-read proved no binding tracks the claim and the bounded
    /// release destroyed it.
    Destroyed,
    /// The release itself failed; the claim is left to its TTL.
    ReleaseFailed,
    /// The release hung past the deadline and was dropped; the claim is
    /// left to its TTL.
    ReleaseTimedOut,
    /// The session's binding still names the claim; nothing was destroyed.
    Spared,
    /// The re-read was inconclusive — it failed, or its value could not be
    /// interpreted as a binding; nothing was destroyed.
    Inconclusive,
}

/// Decide a skipped bind's claim fate, parameterized for tests: `recheck`
/// is the session binding re-read result, `release` destroys the claim,
/// and `deadline` bounds the destroy so a hung Kubernetes API cannot stall
/// the caller's activation error. Every outcome is logged; the result is
/// reported for tests and ignored by the production caller, which always
/// returns the activation error.
pub(super) async fn destroy_untracked_with<T, F, Fut>(
    sandbox_id: &str,
    recheck: Result<Option<Value>, ToolInvokeError>,
    release: F,
    deadline: Duration,
) -> UntrackedClaim
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    match recheck {
        Ok(Some(value)) => match classify_stored_binding(&value, sandbox_id) {
            TracksClaim::Yes => {
                log::info!(
                    "skipped destroying sandbox '{sandbox_id}' after its delayed session \
                     bind did not apply: the session's binding records it"
                );
                return UntrackedClaim::Spared;
            }
            TracksClaim::No => {}
            TracksClaim::Inconclusive => {
                log::warn!(
                    "skipped destroying sandbox '{sandbox_id}' after its delayed session \
                     bind did not apply: its stored binding could not be interpreted"
                );
                return UntrackedClaim::Inconclusive;
            }
        },
        Ok(None) => {}
        Err(error) => {
            log::warn!(
                "skipped destroying sandbox '{sandbox_id}' after its delayed session \
                 bind did not apply: could not re-read the session binding: {error:#}"
            );
            return UntrackedClaim::Inconclusive;
        }
    }
    match tokio::time::timeout(deadline, release()).await {
        Ok(Ok(_)) => {
            log::info!(
                "destroyed untracked sandbox '{sandbox_id}' after its session binding \
                 did not apply"
            );
            UntrackedClaim::Destroyed
        }
        Ok(Err(error)) => {
            log::warn!(
                "failed to release untracked sandbox '{sandbox_id}' after its session \
                 binding did not apply: {error:#}"
            );
            UntrackedClaim::ReleaseFailed
        }
        Err(_) => {
            log::warn!(
                "timed out destroying untracked sandbox '{sandbox_id}' after its session \
                 binding did not apply; its claim TTL will reclaim it"
            );
            UntrackedClaim::ReleaseTimedOut
        }
    }
}

impl LifecycleToolset {
    /// Bind a sandbox that reached activation, so the session records it.
    ///
    /// Unconditional on purpose: this runs only after a successful
    /// activation of the sandbox this connect owns, and the latest
    /// successful connect is the one the session should point at.
    ///
    /// A failure is returned as a recoverable error: the sandbox is usable,
    /// so the caller should be able to retry the bind by reconnecting.
    pub(super) async fn bind_ready_sandbox(
        &self,
        session_id: &str,
        sandbox_id: &str,
    ) -> Result<(), ToolInvokeError> {
        self.gateway
            .bind(session_id, sandbox_id)
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!(
                    "sandbox '{sandbox_id}' is ready but its session binding failed: {error}"
                ))
            })
    }

    /// Report an activation failure for a claimed sandbox. The sandbox is
    /// bound first — so a slow or timed-out activation resumes on the next
    /// connect — unless the claim was proven absent, where there is nothing
    /// to resume and the binding must not record a dead ID.
    ///
    /// The bind is conditional: it applies only while the session's stored
    /// binding is still the raw `observed` value this connect read when it
    /// started, so a delayed bind cannot overwrite one written by a
    /// concurrent or later connect. When the bind does not apply, the
    /// claim is untracked and is destroyed — but only after re-reading the
    /// binding, and never when the bind's commit status is unknown (there
    /// the claim is left to its TTL). Bind and release failures are
    /// logged, never returned, so the caller always sees the activation
    /// failure.
    pub(super) async fn activation_failure(
        &self,
        session_id: &str,
        sandbox_id: &str,
        observed: Option<&Value>,
        error: anyhow::Error,
    ) -> ToolInvokeError {
        if is_sandbox_not_found(&error) {
            return lifecycle_error(error);
        }
        self.bind_unready_sandbox(session_id, sandbox_id, observed)
            .await;
        lifecycle_error(error)
    }

    /// Best-effort bind of a sandbox whose claim exists but is not usable
    /// yet, so a slow or timed-out activation resumes on the next connect.
    ///
    /// The bind applies only while the session's stored binding is still
    /// the raw `observed` value this connect read when it started: a
    /// concurrent or later connect may have bound another sandbox in the
    /// meantime, and a delayed bind must not overwrite it. When the bind is
    /// skipped, the claim is untracked and would leak, so it is destroyed
    /// after [`Self::destroy_untracked_sandbox`] re-checks the binding. A
    /// bind whose commit status is unknown (`Err`) is never followed by a
    /// destroy: the write may have landed, and deleting the claim could
    /// take a live, recorded sandbox with it, so the claim is left to its
    /// TTL. Bind and release failures are logged, never returned, so the
    /// caller still sees the activation failure.
    pub(super) async fn bind_unready_sandbox(
        &self,
        session_id: &str,
        sandbox_id: &str,
        observed: Option<&Value>,
    ) {
        match self
            .gateway
            .bind_if_unchanged(session_id, sandbox_id, observed)
            .await
        {
            Ok(true) => return,
            Ok(false) => log::info!(
                "skipped binding sandbox '{sandbox_id}' to session '{session_id}': \
                 the session's binding changed since the connect started"
            ),
            Err(error) => {
                log::warn!(
                    "failed to bind sandbox '{sandbox_id}' to session '{session_id}' \
                     before reporting its activation failure; leaving the claim to its \
                     TTL because the binding's commit status is unknown: {error:#}"
                );
                return;
            }
        }
        self.destroy_untracked_sandbox(session_id, sandbox_id).await;
    }

    /// Destroy a claim that the session's binding provably does not track.
    ///
    /// The conditional bind was skipped, which usually means a concurrent
    /// connect replaced the binding. Before deleting, re-read the binding:
    /// a duplicate of this same call may have adopted the claim and bound
    /// it — `create` treats 409 as success, so "returned by create" does
    /// not mean "exclusively owned" — and destroying it would take a live
    /// sandbox with it. The re-read inspects the raw stored value, not the
    /// usable binding: a present-but-unusable value that still names this
    /// claim proves the session tracks it. Skip the delete when the
    /// re-read fails too; an inconclusive check must not destroy state.
    ///
    /// The re-read and the delete are not atomic, so a binding can change
    /// in the window between them; the re-read's own request bound (~15s)
    /// plus [`DESTROY_DEADLINE`] means a degraded metadata store can hold
    /// a failed connect's error for roughly a minute before it returns.
    async fn destroy_untracked_sandbox(&self, session_id: &str, sandbox_id: &str) {
        let recheck = self.gateway.raw_binding(Some(session_id)).await;
        // Fresh token on purpose: cleanup must run even when the connect
        // itself was cancelled.
        let cancel = CancellationToken::new();
        destroy_untracked_with(
            sandbox_id,
            recheck,
            || self.gateway.manager.release(sandbox_id, true, &cancel),
            DESTROY_DEADLINE,
        )
        .await;
    }

    /// Remove the session's ambient binding after its sandbox was destroyed.
    ///
    /// Unconditional on purpose: it runs on an explicit `sandbox_release`
    /// of the sandbox the binding records, so the binding must go.
    pub(super) async fn clear_binding(
        &self,
        context: &ToolInvocationContext,
        sandbox_id: &str,
    ) -> Result<(), ToolInvokeError> {
        let Some(session_id) = context.invoking_session_id.as_deref() else {
            return Ok(());
        };
        self.gateway
            .metadata
            .remove_tool_context_value(ToolContextEntry {
                session_id,
                key: SANDBOX_CONTEXT_KEY,
            })
            .await
            .map_err(recoverable)?;
        log::debug!("cleared sandbox '{sandbox_id}' binding from session '{session_id}'");
        Ok(())
    }
}
