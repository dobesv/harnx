//! Shared prompt-specific observer for send.wait and session_wait. Tool discovery
//! and session_load wiring are deliberately separate from these entry seams.
mod budget;
mod load;
mod output;
mod repair;
mod scope;
mod snapshot;

pub(in crate::nats_worker::subagent_toolset) use load::{
    resolve_prompt_scope, PromptScope, PromptScopeError,
};

use super::super::{normalize_session_id, SubagentToolset};
use super::{
    dispatch, parse_send_args, parse_wait_args, DispatchConfirmation, PromptHandle, ResolvedWait,
    SessionPromptResult, SessionPromptStatus, WaitStop,
};
use budget::{Budget, BudgetStops, Stop};
use harnx_toolset::{
    ToolInvocation, ToolInvocationContext, ToolInvokeError, SUBAGENT_SESSION_SEND_TOOL,
    SUBAGENT_SESSION_WAIT_TOOL,
};
use output::Frame;
use scope::{Scope, ScopeError};
use serde_json::Value;
use snapshot::Snapshot;
use tokio_util::sync::CancellationToken;

/// Dispatch remains synchronous through durable acceptance; only observation
/// has a soft budget. No future is spawned or left running when this returns.
pub(in crate::nats_worker::subagent_toolset) async fn send(
    toolset: &SubagentToolset,
    arguments: Value,
    cancel: CancellationToken,
    context: ToolInvocationContext,
) -> Result<Value, ToolInvokeError> {
    let (args, wait) = parse_send_args(arguments.clone()).map_err(ToolInvokeError::Recoverable)?;
    let prospective = PromptHandle {
        session_id: context
            .checkpoint
            .as_ref()
            .and_then(|saved| saved["session_id"].as_str())
            .map(str::to_owned)
            .or_else(|| normalize_session_id(args.session_id))
            .unwrap_or_else(|| "000000".into()),
        prompt_id: context.call_id.clone(),
    };
    let max_bytes = wait.as_ref().map_or(
        super::super::session_read::DEFAULT_MAX_OUTPUT_BYTES as usize,
        |w| w.read.max_output_bytes,
    );
    Frame::new(&prospective, max_bytes).preflight(Some(&context.call_id))?;
    let (dispatched, wait) =
        dispatch::dispatch_send(toolset, arguments, cancel.clone(), context.clone()).await?;
    if let Some(wait) = wait {
        observe(
            toolset,
            Request {
                handle: dispatched.handle,
                wait,
                cancel,
                dispatch: dispatched.dispatch,
            },
            &context,
        )
        .await
    } else {
        let mut frame = Frame::new(&dispatched.handle, max_bytes);
        frame.set_invocation(dispatched.prompt.execution_id());
        frame.result.dispatch = dispatched.dispatch;
        frame.result.status = if dispatched.dispatch.is_some() {
            SessionPromptStatus::Pending
        } else {
            SessionPromptStatus::Unknown
        };
        Ok(frame.finish(&Budget::snapshot(CancellationToken::new())))
    }
}

pub(in crate::nats_worker::subagent_toolset) async fn wait(
    toolset: &SubagentToolset,
    arguments: Value,
    cancel: CancellationToken,
    context: ToolInvocationContext,
) -> Result<Value, ToolInvokeError> {
    let (args, wait) = parse_wait_args(arguments).map_err(ToolInvokeError::Recoverable)?;
    let handle = PromptHandle {
        session_id: args.session_id,
        prompt_id: args.prompt_id,
    };
    observe(
        toolset,
        Request {
            handle,
            wait,
            cancel,
            dispatch: None,
        },
        &context,
    )
    .await
}

/// Route a raw tool name to its observation entry point. Used by the
/// context-less `Toolset::invoke` fallback, where both entries fail closed on
/// the trusted-scope check rather than accepting caller-supplied authority.
pub(in crate::nats_worker::subagent_toolset) async fn send_or_wait(
    toolset: &SubagentToolset,
    invocation: ToolInvocation,
) -> Result<Value, ToolInvokeError> {
    let ToolInvocation {
        tool,
        args,
        cancel,
        context,
    } = invocation;
    match tool.as_str() {
        SUBAGENT_SESSION_SEND_TOOL => send(toolset, args, cancel, context).await,
        SUBAGENT_SESSION_WAIT_TOOL => wait(toolset, args, cancel, context).await,
        other => Err(ToolInvokeError::Recoverable(format!(
            "unknown sub-agent tool: {other}"
        ))),
    }
}

struct Request {
    handle: PromptHandle,
    wait: ResolvedWait,
    cancel: CancellationToken,
    dispatch: Option<DispatchConfirmation>,
}

async fn observe(
    toolset: &SubagentToolset,
    request: Request,
    context: &ToolInvocationContext,
) -> Result<Value, ToolInvokeError> {
    let Request {
        handle,
        wait,
        cancel,
        dispatch,
    } = request;
    let budget = Budget::new(wait.limit, BudgetStops::for_call(cancel.clone(), context));
    // A caller bound already reached must still report what the child is
    // doing: the caller is about to run out of time and may be holding a
    // finished result. Take one bounded pass, as session_load does.
    let single_pass = budget.check() == Err(Stop::CallerDeadline);
    let budget = if single_pass {
        Budget::snapshot(cancel)
    } else {
        budget
    };
    let mut frame = Frame::new(&handle, wait.read.max_output_bytes);
    frame.preflight(None)?;
    frame.result.dispatch = dispatch;
    let scope = match Scope::resolve(toolset, &handle, context, &budget).await {
        Ok(scope) => scope,
        Err(ScopeError::Denied(error)) => return Err(error),
        Err(ScopeError::Stop(reason)) => {
            frame.stopped(reason);
            return Ok(frame.finish(&budget));
        }
    };
    frame.bind(&scope);
    let mut observer = Observer {
        toolset,
        scope,
        snapshot: Snapshot::default(),
        wait,
        budget,
        repaired: false,
        single_pass,
    };
    if let Err(reason) = observer.run(&mut frame).await {
        frame.stopped(reason);
    }
    Ok(frame.finish(&observer.budget))
}

struct Observer<'a> {
    toolset: &'a SubagentToolset,
    scope: Scope,
    snapshot: Snapshot,
    wait: ResolvedWait,
    budget: Budget,
    repaired: bool,
    /// Observe once and stop for the caller's deadline instead of waiting.
    single_pass: bool,
}

impl Observer<'_> {
    async fn run(&mut self, frame: &mut Frame) -> Result<(), Stop> {
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.toolset.jetstream.clone(),
            &self.scope.storage,
        );
        loop {
            let refresh = self.snapshot.refresh(&log, &self.scope, &self.budget).await;
            frame.update(&self.snapshot);
            refresh?;
            // Cache a bounded partial page before sleeping. On expiry/cancellation
            // there is no extra I/O allowance outside the absolute soft deadline.
            frame.render(self).await?;
            if self.single_pass {
                return Err(Stop::CallerDeadline);
            }
            if self.snapshot.settled() {
                return Ok(());
            }
            self.maybe_repair(frame).await?;
            if self.snapshot.settled() {
                frame.render(self).await?;
                return Ok(());
            }
            self.budget.poll().await?;
        }
    }

    async fn maybe_repair(&mut self, frame: &mut Frame) -> Result<(), Stop> {
        let uncertain = frame.result.dispatch != Some(DispatchConfirmation::Confirmed);
        let eligible =
            frame.permits_repair() && self.snapshot.status() == SessionPromptStatus::Pending;
        if self.repaired {
            return Ok(());
        }
        if !uncertain || !eligible {
            return Ok(());
        }
        self.repaired = true;
        let result =
            repair::repair_once(self.toolset, &self.scope, &mut self.snapshot, &self.budget).await;
        // Preserve a terminal found by the repair's recheck even if a subsequent
        // broker operation fails. Repair errors never revoke durable evidence.
        frame.update(&self.snapshot);
        if let Some(dispatch) = result? {
            frame.result.dispatch = Some(dispatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
