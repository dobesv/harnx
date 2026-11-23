//! Resolving and activating the sandbox a connect targets.

use super::lifecycle_toolset::{ConnectArgs, LifecycleToolset};
use super::{lifecycle_error, usable_binding_id};
use crate::lifecycle::is_sandbox_not_found;
use harnx_toolset::{ToolInvocationContext, ToolInvokeError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// How [`LifecycleToolset::resolve_connection`] chose the sandbox for a connect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConnectionSource {
    /// An explicit non-empty `sandbox_id` argument; a missing sandbox is the
    /// caller's error and must not be papered over with a fresh sandbox.
    Explicit,
    /// The session's recorded ambient binding; a missing sandbox means the
    /// binding went stale and can be replaced (#2840).
    Ambient,
    /// A sandbox created fresh by this connect call.
    Created,
}

/// What `SandboxManager::create` needs to name and describe a new claim:
/// the invoking call's ID (which derives the claim name) and the caller's
/// description.
#[derive(Clone, Copy)]
pub(super) struct ClaimRequest<'a> {
    pub(super) call_id: &'a str,
    pub(super) description: Option<&'a str>,
}

/// A connect's resolved target: which sandbox to activate, how it was
/// chosen, the claim request for a fresh create, and the session's raw
/// stored binding observed when the connect started — the precondition a
/// delayed unready bind must still find in place, exactly as stored, so a
/// present-but-unusable binding is replaced rather than treated as absent.
pub(super) struct Connection<'a> {
    sandbox_id: String,
    source: ConnectionSource,
    observed: Option<Value>,
    claim: ClaimRequest<'a>,
}

impl<'a> Connection<'a> {
    fn new(
        source: ConnectionSource,
        sandbox_id: String,
        observed: Option<Value>,
        claim: ClaimRequest<'a>,
    ) -> Self {
        Self {
            source,
            sandbox_id,
            observed,
            claim,
        }
    }
}

impl LifecycleToolset {
    /// Resolve which sandbox this connect targets, together with the
    /// session's raw stored binding at connect start: the precondition a
    /// delayed unready bind must still find in place, carried exactly as
    /// stored (see [`Connection`]). If another connect replaced the
    /// binding in the meantime, this connect's bind is skipped and its
    /// claim handled instead of overwriting the newer binding.
    pub(super) async fn resolve_connection<'a>(
        &self,
        args: &'a ConnectArgs,
        context: &'a ToolInvocationContext,
        cancel: &CancellationToken,
    ) -> Result<Connection<'a>, ToolInvokeError> {
        let claim = ClaimRequest {
            call_id: &context.call_id,
            description: args.description.as_deref(),
        };
        // An explicit non-empty ID wins and never reads the binding.
        if let Some(id) = args.sandbox_id.as_ref().filter(|id| !id.trim().is_empty()) {
            return Ok(Connection::new(
                ConnectionSource::Explicit,
                id.clone(),
                None,
                claim,
            ));
        }
        // Omitted or blank ID: reuse the session's bound sandbox (#2633) and
        // claim a new one only when no usable binding exists. Liveness is
        // checked later by `ensure_active`, which lets connect replace a
        // stale binding (#2840).
        let observed = self
            .gateway
            .raw_binding(context.invoking_session_id.as_deref())
            .await?;
        if let Some(sandbox_id) = usable_binding_id(observed.as_ref())? {
            return Ok(Connection::new(
                ConnectionSource::Ambient,
                sandbox_id,
                observed,
                claim,
            ));
        }
        let sandbox_id = self
            .gateway
            .manager
            .create(claim.call_id, claim.description, cancel)
            .await
            .map_err(lifecycle_error)?;
        Ok(Connection::new(
            ConnectionSource::Created,
            sandbox_id,
            observed,
            claim,
        ))
    }

    /// Ensure the resolved sandbox is active, returning its ID and pod IP.
    ///
    /// A sandbox that came from the session's ambient binding and no longer
    /// exists (its claim expired or was deleted) is replaced with a fresh one
    /// so an ID-less connect recovers instead of failing forever (#2840).
    /// A freshly created sandbox whose activation fails is still bound to the
    /// session (unless its claim is proven absent, or the session's binding
    /// moved on), so the next connect resumes it instead of minting a new
    /// claim on every retry.
    pub(super) async fn activate_sandbox(
        &self,
        session_id: &str,
        connection: &Connection<'_>,
        cancel: &CancellationToken,
    ) -> Result<(String, String), ToolInvokeError> {
        let Connection {
            sandbox_id,
            source,
            observed,
            claim,
        } = connection;
        match (
            source,
            self.gateway.manager.ensure_active(sandbox_id, cancel).await,
        ) {
            (_, Ok(pod_ip)) => Ok((sandbox_id.to_string(), pod_ip)),
            (ConnectionSource::Ambient, Err(error)) if is_sandbox_not_found(&error) => {
                self.replace_missing_sandbox(
                    session_id,
                    sandbox_id,
                    observed.as_ref(),
                    *claim,
                    cancel,
                )
                .await
            }
            (ConnectionSource::Created, Err(error)) => Err(self
                .activation_failure(session_id, sandbox_id, observed.as_ref(), error)
                .await),
            (_, Err(error)) => Err(lifecycle_error(error)),
        }
    }

    /// Disconnect the vanished sandbox's MCP session, claim a fresh sandbox,
    /// activate it, and bind it after activation (or on failure unless its
    /// claim is proven absent or the session's binding moved on in the
    /// meantime). Only reached for ambient bindings whose activation found
    /// the claim absent; transient, timeout, auth, and cancellation errors
    /// never get here and leave the binding untouched.
    ///
    /// The replacement is claimed under a fresh name (`create` derives it from
    /// this call's ID), never the old name: the old claim's `Delete` shutdown
    /// policy destroyed its storage, so nothing can be preserved, and
    /// `create_claim` treats 409 as success, so a same-named claim still
    /// terminating asynchronously would be reported as created (#2840).
    async fn replace_missing_sandbox(
        &self,
        session_id: &str,
        old_sandbox_id: &str,
        observed: Option<&Value>,
        claim: ClaimRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<(String, String), ToolInvokeError> {
        log::warn!("session's sandbox '{old_sandbox_id}' no longer exists; creating a replacement");
        self.gateway.caller.disconnect(old_sandbox_id).await;
        let sandbox_id = self
            .gateway
            .manager
            .create(claim.call_id, claim.description, cancel)
            .await
            .map_err(lifecycle_error)?;
        // The stale ambient ID is the binding this connect observed at its
        // start, so the delayed bind replaces exactly it and nothing newer.
        self.ensure_bound(session_id, sandbox_id, observed, cancel)
            .await
    }

    /// Activate a just-claimed sandbox. On activation failure, bind it unless
    /// its claim is proven absent or the session's binding changed since this
    /// connect observed it; this lets a slow or timed-out activation resume
    /// on the next connect instead of leaking the claim. A replacement that
    /// later vanishes goes through the same recovery again (#2840).
    async fn ensure_bound(
        &self,
        session_id: &str,
        sandbox_id: String,
        observed: Option<&Value>,
        cancel: &CancellationToken,
    ) -> Result<(String, String), ToolInvokeError> {
        match self
            .gateway
            .manager
            .ensure_active(&sandbox_id, cancel)
            .await
        {
            Ok(pod_ip) => Ok((sandbox_id, pod_ip)),
            Err(error) => Err(self
                .activation_failure(session_id, &sandbox_id, observed, error)
                .await),
        }
    }
}
