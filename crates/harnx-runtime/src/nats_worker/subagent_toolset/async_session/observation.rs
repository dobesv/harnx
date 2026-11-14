//! Shared prompt-specific observer for send.wait and session_wait. Tool discovery
//! and session_load wiring are deliberately separate from these entry seams.
mod budget;
mod load;
mod output;
mod repair;
mod scope;
mod snapshot;

/// The snapshot pass bound the watcher's durable scan uses by default
/// (re-exported for `async_session::progress`, which pins it into
/// `WatcherConfig`).
pub(in crate::nats_worker::subagent_toolset) use budget::{Stop, IO_LIMIT};
/// Test seam (R6-B1): force the next bound watch scan's first refreshes to
/// fail transiently, standing in for a broker read failure a live test
/// server cannot produce deterministically.
#[cfg(test)]
pub(in crate::nats_worker::subagent_toolset) use load::set_forced_refresh_failures;
pub(in crate::nats_worker::subagent_toolset) use load::{
    resolve_prompt_scope, PromptScope, PromptScopeError, WatchBindError, WatchScan, WatchState,
    WatchTarget,
};

use super::super::{normalize_session_id, SubagentToolset};
use super::{
    dispatch, parse_send_args, parse_wait_args, progress, DispatchConfirmation, PromptHandle,
    ResolvedWait, SessionPromptResult, SessionPromptStatus, WaitStop,
};
use crate::nats_worker::subagent_progress::SharedSnapshots;
use budget::{Budget, BudgetStops};
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
    // The prompt is durably admitted and activated: announce it as Running
    // right away. The watcher follows it to durable settlement even when the
    // caller never waits, and its keying deduplicates the one `observe` starts
    // below for the wait path.
    if let Ok(parent) = dispatch::trusted_parent_scope(&context) {
        progress::ensure_watcher(
            toolset,
            progress::WatchedPrompt {
                parent,
                agent: toolset.agent.clone(),
                session_id: dispatched.handle.session_id.clone(),
                prompt_id: dispatched.handle.prompt_id.clone(),
                tool_call_id: args.tool_call_id.clone(),
            },
        );
    }
    if let Some(wait) = wait {
        observe(
            toolset,
            Request {
                handle: dispatched.handle,
                wait,
                cancel,
                dispatch: dispatched.dispatch,
                tool_call_id: args.tool_call_id.clone(),
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
    // Same cross-check as the dispatch path: a model-supplied value was
    // scrubbed server-side, so a surviving disagreement means the transport
    // itself is inconsistent.
    if let (Ok(parent_key), Some(arg)) = (
        dispatch::trusted_parent_scope(&context),
        args.parent_session_id.as_ref(),
    ) {
        if arg != &parent_key {
            return Err(ToolInvokeError::Recoverable(
                "parent session argument disagrees with trusted storage scope".into(),
            ));
        }
    }
    // The transport injects the parent transcript's call id as a hidden
    // argument; the watcher labels the child's row with it (F4). Model-supplied
    // values were scrubbed server-side before this parse.
    let tool_call_id = args.tool_call_id.clone();
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
            tool_call_id,
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
    /// Parent transcript's tool call id for the wait invocation, when the
    /// transport supplied one (`session_wait` gets it as a hidden argument).
    tool_call_id: Option<String>,
}

/// The live-progress watcher for an authorized wait, plus the shared cell it
/// publishes to. When the same process dispatched this prompt the registry
/// keeps the dispatch's watcher (with its transport-supplied tool call id);
/// otherwise this starts one, so a restarted parent still sees the child it
/// is waiting on. The returned Arc outlives the registry entry: the watcher
/// may unregister itself before the settled summary reads the cell.
fn wait_watcher(
    toolset: &SubagentToolset,
    context: &ToolInvocationContext,
    frame: &Frame,
    tool_call_id: Option<String>,
) -> Option<SharedSnapshots> {
    dispatch::trusted_parent_scope(context).ok().map(|parent| {
        progress::ensure_watcher(
            toolset,
            progress::WatchedPrompt {
                parent,
                agent: toolset.agent.clone(),
                session_id: frame.result.session_id.clone(),
                prompt_id: frame.result.prompt_id.clone(),
                tool_call_id,
            },
        )
    })
}

/// The wait's observation budget: single-pass when the caller's deadline is
/// already reached (the caller still deserves one bounded page, as
/// `session_load` takes), otherwise the full soft budget for the wait.
fn observation_budget(
    wait: &ResolvedWait,
    cancel: CancellationToken,
    context: &ToolInvocationContext,
) -> (Budget, bool) {
    let budget = Budget::new(wait.limit, BudgetStops::for_call(cancel.clone(), context));
    let single_pass = budget.check() == Err(Stop::CallerDeadline);
    let budget = if single_pass {
        Budget::snapshot(cancel)
    } else {
        budget
    };
    (budget, single_pass)
}

/// Close out a settled observation: the frame's result value plus the durable
/// terminal summary attached to it, merged with the live watcher's last
/// published snapshot. Everything the summary needs except the watcher share
/// lives on the observer (toolset, wait budget ceiling); synchronous —
/// publishing here is the caller's job.
fn finish_with_terminal_progress(
    observer: &Observer<'_>,
    frame: Frame,
    watcher_share: Option<&SharedSnapshots>,
) -> Value {
    let toolset = observer.toolset;
    let max_bytes = observer.wait.read.max_output_bytes;
    let terminal = observer.snapshot.terminal_evidence();
    let observed = (
        frame.result.session_id.clone(),
        frame.result.prompt_id.clone(),
    );
    let mut value = frame.finish(&observer.budget);
    if let Some((status, usage)) = terminal {
        // Merge the live watcher's last published snapshot: title, tool count
        // and accumulated usage carry over, so the durable summary cannot
        // lose a first-terminal-wins race against a richer live view (F12),
        // and `elapsed_ms` stays the child's runtime rather than this wait's
        // duration (F11). Read the cell held since `ensure_watcher`, not the
        // registry: the watcher may have exited and unregistered itself
        // before this summary is built.
        let live = watcher_share.and_then(|shared| shared.lock().expect("watcher share").clone());
        attach_terminal_progress(
            &mut value,
            toolset,
            TerminalSummary {
                child: progress::ChildIdentity {
                    agent: toolset.agent.clone(),
                    session_id: observed.0,
                    prompt_id: observed.1,
                },
                status,
                usage,
                live,
                max_bytes,
            },
        );
    }
    value
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
        tool_call_id,
    } = request;
    let (budget, single_pass) = observation_budget(&wait, cancel, context);
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
    let watcher_share = wait_watcher(toolset, context, &frame, tool_call_id);
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
    let value = finish_with_terminal_progress(&observer, frame, watcher_share.as_ref());
    Ok(value)
}

/// Durable terminal evidence plus the live view a settled observation
/// attaches to its tool result: the child's identity, the classification
/// with its `TurnEnd` total, the watcher's last published snapshot, and the
/// caller's output budget — everything the summary needs in one bundle.
struct TerminalSummary {
    child: progress::ChildIdentity,
    status: SessionPromptStatus,
    usage: Option<harnx_core::api_types::CompletionTokenUsage>,
    live: Option<harnx_core::event::SubAgentProgress>,
    max_bytes: usize,
}

/// Durable terminal summary for replay: `completed_subagent_progress`
/// (nats_session/completion.rs) recovers the child's row when live progress
/// events were lost, and the TUI's history rebuild needs the `sub_agent`
/// marker to restore it (`subagent_key_from_output`, lifecycle.rs) — the
/// sync path writes the same pair at termination (`termination::result_value`).
/// Only a settled observation carries either — a dispatch receipt and an
/// expired wait never do — and presentation is expendable, so both fields are
/// dropped when they would overflow the caller's output budget.
fn attach_terminal_progress(
    value: &mut Value,
    toolset: &SubagentToolset,
    summary: TerminalSummary,
) {
    let TerminalSummary {
        child,
        status,
        usage,
        live,
        max_bytes,
    } = summary;
    let progress = progress::terminal_progress_value(&child, status, usage, live);
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.insert(
        "sub_agent".to_owned(),
        serde_json::json!({
            "agent": toolset.agent,
            "session_id": child.session_id,
        }),
    );
    object.insert(
        "sub_agent_progress".to_owned(),
        serde_json::to_value(&progress).expect("sub-agent progress serializes"),
    );
    // Presentation is expendable: drop the progress first, then the marker, if
    // either would overflow the caller's output budget.
    if serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() > max_bytes) {
        value
            .as_object_mut()
            .expect("progress object")
            .remove("sub_agent_progress");
        if serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() > max_bytes) {
            value
                .as_object_mut()
                .expect("progress object")
                .remove("sub_agent");
        }
    }
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
