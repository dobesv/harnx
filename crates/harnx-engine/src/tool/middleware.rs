//! Runtime-owned middleware wraps authorized calls without owning UI publication.

use super::{
    dispatch_authorized_call, ApprovedToolCall, InnerToolCallStatus, ToolEvalContext,
    ToolEvaluation,
};
use anyhow::{anyhow, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::tool::{ToolCall, ToolError, ToolResult};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch, Notify};

mod outcome;
use outcome::finish_middleware;
pub(super) use outcome::materialize_dispatch_result;

/// Evaluation entry point, distinct from the provider's transcript identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolMiddlewareOrigin {
    Model,
    Inner,
    Operator,
}

/// Authorized input after argument parsing, legacy PreToolUse, and approval.
#[derive(Debug, Clone)]
pub struct PreparedToolCall {
    /// Original transcript identity and arguments. `arguments` below contains
    /// parsed arguments, including any legacy hook mutation.
    pub call: ToolCall,
    pub arguments: Value,
    /// Actual invocation context, including late-installed lease/admission guards.
    /// Runtime uses this for trusted allowlisted calls, never script-visible input.
    pub context: ToolMiddlewareContext,
}

#[derive(Clone)]
pub struct ToolMiddlewareContext {
    pub evaluation: Arc<ToolEvalContext>,
    pub abort: AbortSignal,
}

impl std::fmt::Debug for ToolMiddlewareContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolMiddlewareContext")
            .finish_non_exhaustive()
    }
}

/// Full downstream result, including private execution observations and media.
/// Never serialize this to build a script-visible envelope: serialization drops
/// private fields that a host-owned result snapshot must retain.
#[derive(Debug, Clone)]
pub struct MaterializedToolResult {
    pub result: ToolResult,
    pub status: InnerToolCallStatus,
    // Preserve the legacy null UI payload even though model results use DONE.
    normalized_null: bool,
}

impl MaterializedToolResult {
    /// Construct a synthetic or transformed result without running post-use hooks.
    pub fn new(result: ToolResult, status: InnerToolCallStatus) -> Self {
        Self {
            result,
            status,
            normalized_null: false,
        }
    }

    pub(super) fn was_null(&self) -> bool {
        self.result.output.is_null()
            || (self.normalized_null && self.result.output == json!("DONE"))
    }
}

/// Control-flow outcome rather than script-authored JSON. InputRequired stops
/// the chain without becoming a catchable error or invoking on_error.
#[derive(Debug, Clone)]
#[non_exhaustive]
// Preserve the Phase 1 by-value Completed API for middleware implementors.
#[allow(clippy::large_enum_variant)]
pub enum ToolMiddlewareOutcome {
    Completed(MaterializedToolResult),
    InputRequired {
        interaction: harnx_core::interaction::InteractionRequest,
        checkpoint: super::ToolCallCheckpoint,
    },
}

pub type ToolMiddlewareFuture =
    Pin<Box<dyn Future<Output = Result<ToolMiddlewareOutcome, ToolError>> + Send>>;

/// Each invocation executes a fresh dispatch with these effective arguments.
/// Valid only while the middleware callback is running. No terminal result event
/// or handoff activation occurs inside this continuation.
pub type ToolCallNext = dyn Fn(Value) -> ToolMiddlewareFuture + Send + Sync;

/// Runtime supplies the chain runner; the engine has no script dependency.
pub type ToolMiddlewareFn = dyn Fn(PreparedToolCall, ToolMiddlewareOrigin, Arc<ToolCallNext>) -> ToolMiddlewareFuture
    + Send
    + Sync;

#[derive(Default)]
struct TerminalControl {
    error: Mutex<Option<anyhow::Error>>,
    ready: Notify,
}

impl TerminalControl {
    fn take(&self) -> Option<anyhow::Error> {
        self.error.lock().take()
    }

    fn record(&self, error: anyhow::Error) {
        let mut terminal = self.error.lock();
        if terminal.is_none() {
            *terminal = Some(error);
        }
        self.ready.notify_one();
    }
}

fn inactive_continuation() -> ToolError {
    ToolError::Fatal(anyhow!("tool middleware continuation is no longer active"))
}

// qual:allow(srp, god_struct) reason: "One owned continuation keeps dispatch inputs and lifetime/terminal guards together so it cannot outlive its callback."
#[derive(Clone)]
struct NextDispatch {
    ctx: Arc<ToolEvalContext>,
    abort: AbortSignal,
    call: ToolCall,
    lossless: bool,
    origin: ToolMiddlewareOrigin,
    active: watch::Receiver<()>,
    authorize: Option<mpsc::UnboundedSender<oneshot::Sender<()>>>,
    terminal: Arc<TerminalControl>,
}

impl NextDispatch {
    fn call(&self, arguments: Value) -> ToolMiddlewareFuture {
        let dispatch = self.clone();
        Box::pin(async move { dispatch.run(arguments).await })
    }

    async fn run(self, arguments: Value) -> Result<ToolMiddlewareOutcome, ToolError> {
        if self.active.has_changed().is_err() {
            return Err(inactive_continuation());
        }
        let mut active = self.active.clone();
        let result = tokio::select! {
            biased;
            _ = active.changed() => return Err(inactive_continuation()),
            result = self.dispatch(arguments) => result,
        };
        match result {
            Ok(result) => Ok(ToolMiddlewareOutcome::Completed(result)),
            Err(ToolError::Fatal(error)) => {
                // Keep the original typed cause out of the script's error
                // policy. Even an ignored next error cannot swallow it.
                self.terminal.record(error);
                Err(ToolError::Fatal(anyhow!(
                    "downstream tool control flow interrupted middleware"
                )))
            }
            Err(error) => Err(error),
        }
    }

    // qual:allow(iosp) reason: "Cancellation and the optional borrowed-authorization bridge must precede leaf dispatch in the host operation's task."
    async fn dispatch(&self, arguments: Value) -> Result<MaterializedToolResult, ToolError> {
        if self.abort.aborted() {
            return Err(ToolError::Fatal(anyhow!(
                "interrupted during tool execution"
            )));
        }
        if let Some(authorize) = &self.authorize {
            let (reply, granted) = oneshot::channel();
            authorize.send(reply).map_err(|_| inactive_continuation())?;
            granted.await.map_err(|_| inactive_continuation())?;
        }
        let mut call = self.call.clone();
        call.arguments = arguments.clone();
        let effective = ApprovedToolCall {
            tool_use_id: call.id.clone().unwrap_or_default(),
            call,
            json_data: arguments.clone(),
            tool_input: arguments,
        };
        let output = dispatch_authorized_call(&effective, &self.ctx, &self.abort, None).await;
        materialize_dispatch_result(
            &ToolEvaluation {
                ctx: &self.ctx,
                abort_signal: &self.abort,
                authorization: None,
                lossless: self.lossless,
                origin: self.origin,
            },
            &effective,
            output,
        )
        .await
    }
}

/// The leaf runs in the caller's task, preserving script nesting/admission
/// context across host-command awaits. Only borrowed replay authorization needs
/// a request bridge back into the engine task. The watch sender scopes owned
/// continuations to this callback, including continuations moved into spawned tasks.
// qual:allow(dry, boilerplate) reason: "The owned next closure needs independent context, cancellation and call snapshots while the borrowed evaluation drives authorization."
pub(super) async fn run_middleware(
    evaluation: ToolEvaluation<'_>,
    approved: &ApprovedToolCall,
    middleware: &ToolMiddlewareFn,
) -> Result<ToolMiddlewareOutcome, ToolError> {
    let (send, receive) = mpsc::unbounded_channel();
    let (_active, active) = watch::channel(());
    let terminal = Arc::new(TerminalControl::default());
    let dispatch = NextDispatch {
        ctx: Arc::new(evaluation.ctx.clone()),
        abort: evaluation.abort_signal.clone(),
        call: approved.call.clone(),
        lossless: evaluation.lossless,
        origin: evaluation.origin,
        active,
        authorize: evaluation.authorization.map(|_| send),
        terminal: terminal.clone(),
    };
    let next = Arc::new(move |arguments| dispatch.call(arguments));
    let chain = middleware(
        PreparedToolCall {
            call: approved.call.clone(),
            arguments: approved.json_data.clone(),
            context: ToolMiddlewareContext {
                evaluation: Arc::new(evaluation.ctx.clone()),
                abort: evaluation.abort_signal.clone(),
            },
        },
        evaluation.origin,
        next,
    );
    drive_middleware(evaluation, approved, chain, terminal, receive).await
}

async fn drive_middleware(
    evaluation: ToolEvaluation<'_>,
    approved: &ApprovedToolCall,
    mut chain: ToolMiddlewareFuture,
    terminal: Arc<TerminalControl>,
    mut receive: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
) -> Result<ToolMiddlewareOutcome, ToolError> {
    let mut pending_authorizations = FuturesUnordered::new();
    loop {
        tokio::select! {
            outcome = &mut chain => {
                if let Some(error) = terminal.take() {
                    return Err(ToolError::Fatal(error));
                }
                return finish_middleware(approved, outcome);
            }
            Some(reply) = receive.recv(), if evaluation.authorization.is_some() => {
                pending_authorizations.push(async move {
                    let result = match evaluation.authorization {
                        Some(authorization) => authorization.revalidate().await,
                        None => Ok(()),
                    };
                    (reply, result)
                });
            }
            Some((reply, result)) = pending_authorizations.next(), if !pending_authorizations.is_empty() => {
                result.map_err(ToolError::Fatal)?;
                let _ = reply.send(());
            }
            _ = terminal.ready.notified() => {
                if let Some(error) = terminal.take() {
                    return Err(ToolError::Fatal(error));
                }
            }
            _ = wait_abort_signal(evaluation.abort_signal) => {
                let error = terminal.take().unwrap_or_else(|| anyhow!("interrupted during tool execution"));
                return Err(ToolError::Fatal(error));
            }
        }
    }
}
