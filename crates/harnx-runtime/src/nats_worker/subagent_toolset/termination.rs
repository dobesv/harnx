use super::{CompletedSubagentTurn, ProgressReporterStart, SubagentToolset};
use crate::nats_session::{NatsSession, NatsTurnResult};
use crate::{
    parse_worker_terminal, synthesize_terminated_result, InvocationBufferingSink, RunTurnOptions,
    SynthesizedResult, TerminationInputs, TerminationKind,
};
use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};
use harnx_core::message::MessageContent;
use harnx_toolset::ToolInvokeError;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "expired_replay_tests.rs"]
mod expired_replay_tests;

#[cfg(test)]
mod historical_terminal_tests;

pub(super) fn subagent_error_message(prefix: impl std::fmt::Display, session_id: &str) -> String {
    format!("{prefix} (session_id: {session_id})")
}

pub(super) struct PromptParams {
    pub content: MessageContent,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub timeout_secs: Option<u64>,
    pub cancel: CancellationToken,
    pub context: harnx_toolset::ToolInvocationContext,
}

pub(super) async fn run_prompt(
    toolset: &SubagentToolset,
    params: PromptParams,
) -> Result<CompletedSubagentTurn, ToolInvokeError> {
    if params.cancel.is_cancelled() {
        return Err(ToolInvokeError::Fatal("sub-agent tool call aborted".into()));
    }
    let cancel = params.cancel.clone();
    let lineage = params.context.run_context.as_ref().ok_or_else(|| {
        ToolInvokeError::Fatal("sub-agent execution requires inherited run context".into())
    })?;
    let parent: crate::nats_session_metadata::RunLimitsRecord =
        serde_json::from_value(lineage.snapshot.clone()).map_err(|error| {
            ToolInvokeError::Fatal(format!("invalid inherited run context: {error}"))
        })?;
    let admitted_at = chrono::DateTime::from_timestamp_millis(
        i64::try_from(lineage.started_at_ms)
            .map_err(|error| ToolInvokeError::Fatal(error.to_string()))?,
    )
    .ok_or_else(|| ToolInvokeError::Fatal("invalid original invocation time".into()))?;
    let session = checkpointed_session(toolset, &params).await?;
    let session = session.with_inherited_admission(
        parent,
        params.context.call_id.clone(),
        admitted_at,
        crate::nats_session_metadata::InvocationEdgeKind::Delegation,
        params.timeout_secs,
    );
    record_child(toolset, &params.context, session.session_id()).await;
    // Replay of an already-answered invocation keys off
    // `NatsSession::invocation_id`, so every child needs one bound here. The
    // parent session id rides along only to label who requested an interrupt
    // the child appends; it carries no authority of its own.
    let session = match params.context.invoking_session_id.clone() {
        Some(parent) => session.with_execution_parent(parent, params.context.call_id.clone()),
        None => session,
    };
    let session = Arc::new(session);
    let child_session_id = session.session_id().to_string();
    let reporter = toolset
        .start_progress_reporter(ProgressReporterStart {
            child_session_id: child_session_id.clone(),
            parent_session_id: params.parent_session_id,
            invocation_id: params.context.call_id.clone(),
            tool_call_id: params.tool_call_id,
        })
        .await;
    let buffering_sink = Arc::new(InvocationBufferingSink::new(reporter.sink()));
    let turn = await_prompt_turn(
        &session,
        &buffering_sink,
        AwaitTurnParams {
            content: params.content,
            cancel: params.cancel,
        },
    )
    .await;

    // A late child completion cannot resume the stopped parent.
    if cancel.is_cancelled() {
        let _ = reporter.finish(SubAgentProgressStatus::Cancelled).await;
        return Err(ToolInvokeError::Fatal("sub-agent tool call aborted".into()));
    }
    match turn {
        PromptTurn::Completed(Ok(result)) => {
            finish_completed_turn(CompletedTurnParams {
                toolset,
                child_session_id,
                reporter,
                buffering_sink,
                result,
            })
            .await
        }
        PromptTurn::Completed(Err(error)) | PromptTurn::Aborted(error) => {
            if let Err(report_error) = reporter.finish(SubAgentProgressStatus::Failed).await {
                log::debug!("failed to publish terminal sub-agent progress: {report_error:#}");
            }
            Err(error)
        }
    }
}

async fn checkpointed_session(
    toolset: &SubagentToolset,
    params: &PromptParams,
) -> Result<NatsSession, ToolInvokeError> {
    let parent = params.parent_session_id.as_deref();
    let mut session_id = params
        .context
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint["session_id"].as_str())
        .map(str::to_string)
        .or_else(|| params.session_id.clone());
    if session_id.is_none() {
        let context = params.context.run_context.as_ref().ok_or_else(|| {
            ToolInvokeError::Fatal("child allocation requires inherited context".into())
        })?;
        let config = toolset
            .session_config(None, parent, params.tool_call_id.as_deref())
            .await?;
        session_id = Some(
            crate::utils::session_name::reserve_invocation_session_id(
                &toolset.session_metadata,
                &config.initializer,
                &params.context.call_id,
                context.started_at_ms,
            )
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!("reserve child admission: {error:#}"))
            })?,
        );
    }
    let session = toolset
        .create_session(session_id, parent, params.tool_call_id.as_deref())
        .await?;
    let Some(parent) = parent else {
        return Ok(session);
    };
    let Some(store) = params.context.checkpoint_store.as_ref() else {
        return Ok(session);
    };
    // First writer wins: a concurrent or replayed attempt converges on
    // whichever child id landed first, instead of running a second one.
    let stored = store
        .checkpoint(serde_json::json!({"session_id": session.session_id(), "storage_key": session.storage_key(), "cluster": toolset.route.cluster()}))
        .await
        .map_err(|error| {
            ToolInvokeError::Fatal(format!("persist sub-agent checkpoint: {error:#}"))
        })?;
    let id = stored["session_id"]
        .as_str()
        .ok_or_else(|| ToolInvokeError::Fatal("invalid sub-agent checkpoint".into()))?;
    if id == session.session_id() {
        return Ok(session);
    }
    toolset
        .create_session(
            Some(id.into()),
            Some(parent),
            params.tool_call_id.as_deref(),
        )
        .await
}

/// Name the child as the call's partial result, so a call that fails, times
/// out or is interrupted from here on still tells the parent which session
/// ran. Losing the record costs the parent that name, not the call.
async fn record_child(
    toolset: &SubagentToolset,
    context: &harnx_toolset::ToolInvocationContext,
    session_id: &str,
) {
    let partial_result = serde_json::json!({
        "session_id": session_id,
        "sub_agent": child_source(toolset, session_id),
    });
    if let Err(error) = context.record_partial_result(partial_result).await {
        log::warn!("record sub-agent session {session_id} as the call's partial result: {error:#}");
    }
}

fn child_source(toolset: &SubagentToolset, session_id: &str) -> harnx_core::event::AgentSource {
    harnx_core::event::AgentSource {
        agent: toolset.agent.clone(),
        session_id: Some(session_id.to_string()),
        model: None,
    }
}

struct AwaitTurnParams {
    content: MessageContent,
    cancel: CancellationToken,
}

struct AwaitCompletionParams {
    cancel: CancellationToken,
}

async fn await_prompt_turn(
    session: &NatsSession,
    buffering_sink: &Arc<InvocationBufferingSink>,
    params: AwaitTurnParams,
) -> PromptTurn {
    // The child can finish before its tool-server reply reaches the journal.
    // Read that durable result before an already-expired replay can cancel it.
    match session.completed_invocation_turn().await {
        Ok(Some(result)) => return PromptTurn::Completed(Ok(result)),
        Ok(None) => {}
        Err(error) => {
            return PromptTurn::Completed(Err(ToolInvokeError::Recoverable(
                subagent_error_message(
                    format_args!("read completed sub-agent turn: {error:#}"),
                    session.session_id(),
                ),
            )))
        }
    }
    let AwaitTurnParams { content, cancel } = params;
    let (cancel_tx, cancel_rx) = mpsc::channel(1);
    let child = session.clone();
    let sink = buffering_sink.clone();
    // Worker owns the frozen deadline, including target defaults and inheritance.
    // A second caller timer could misclassify the winning deadline scope.
    // See `AGENTS.md` under "Run-deadline cancellation is invocation-fenced".
    let options = RunTurnOptions::default();
    let run_turn = tokio::spawn(async move {
        child
            .run_turn_content_with_options(content, sink, Some(cancel_rx), options)
            .await
    });
    await_owned_turn(
        session,
        run_turn,
        cancel_tx,
        AwaitCompletionParams { cancel },
    )
    .await
}

async fn await_owned_turn(
    session: &NatsSession,
    mut run_turn: tokio::task::JoinHandle<anyhow::Result<NatsTurnResult>>,
    cancel_tx: mpsc::Sender<()>,
    params: AwaitCompletionParams,
) -> PromptTurn {
    let turn = tokio::select! {
        result = &mut run_turn => PromptTurn::Completed(result.unwrap_or_else(|error| Err(error.into())).map_err(|error| {
            ToolInvokeError::Recoverable(subagent_error_message(
                format_args!("run sub-agent turn: {error:#}"),
                session.session_id(),
            ))
        })),
        _ = params.cancel.cancelled() => {
            let _ = cancel_tx.try_send(());
            supervise_turn(run_turn);
            let _ = ensure_parent_cancellation(session).await;
            PromptTurn::Aborted(ToolInvokeError::Fatal(
                "sub-agent tool call aborted".to_string(),
            ))
        }

    };

    turn
}

/// Let an abandoned turn's follower run itself out. The tool call has already
/// returned a timeout or abort result, so nothing is waiting on this one; the
/// follower only reads the child session's log and its own event stream, which
/// is why leaving it detached cannot corrupt anything behind our back.
fn supervise_turn(turn: tokio::task::JoinHandle<anyhow::Result<NatsTurnResult>>) {
    tokio::spawn(async move {
        let _ = turn.await;
    });
}

async fn ensure_parent_cancellation(session: &NatsSession) -> Result<(), ToolInvokeError> {
    let session_id = session.session_id();
    session
        .interrupt_admitted_invocation("parent interrupted")
        .await
        .map_err(|error| {
            parent_cancellation_error(
                session_id,
                format!("cancellation request failed: {error:#}"),
            )
        })?;
    Ok(())
}

fn parent_cancellation_error(session_id: &str, reason: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!(
        "sub-agent abort: durable cancellation could not be confirmed for session '{session_id}'; not safe to retry: {reason}"
    ))
}

struct CompletedTurnParams<'a> {
    toolset: &'a SubagentToolset,
    child_session_id: String,
    reporter: super::SubagentProgressReporter,
    buffering_sink: Arc<InvocationBufferingSink>,
    result: NatsTurnResult,
}

async fn finish_completed_turn(
    params: CompletedTurnParams<'_>,
) -> Result<CompletedSubagentTurn, ToolInvokeError> {
    let cancelled =
        params.result.was_cancelled || params.toolset.turn_has_cancel(&params.result).await;
    let spec = completed_termination_spec(&params.result);
    let budget_exceeded = spec
        .as_ref()
        .is_some_and(|spec| spec.kind == TerminationKind::BudgetExceeded);
    let status = completed_progress_status(&params.result, cancelled, budget_exceeded);
    let progress = finish_progress(&params.reporter, status).await?;
    if cancelled
        && spec
            .as_ref()
            .is_none_or(|s| s.kind != TerminationKind::Timeout)
    {
        return Err(ToolInvokeError::Recoverable(subagent_error_message(
            "sub-agent turn was cancelled",
            &params.child_session_id,
        )));
    }
    let durable_progress = if spec.is_some() {
        load_public_progress(params.toolset, &params.result).await
    } else {
        None
    };
    let termination = spec.map(|spec| {
        synthesize_termination(
            spec,
            &params.child_session_id,
            &progress,
            &params.buffering_sink,
            durable_progress.as_ref(),
        )
    });
    Ok(CompletedSubagentTurn {
        session_id: params.child_session_id,
        result: Some(params.result),
        progress,
        termination,
    })
}

/// The termination a completed child turn reports, if its error is a
/// worker-side stop rather than an ordinary failure.
fn completed_termination_spec(result: &NatsTurnResult) -> Option<TerminationSpec> {
    result
        .error
        .as_deref()
        .and_then(parse_worker_terminal)
        .map(|terminal| TerminationSpec {
            kind: terminal.kind(),
            budget: terminal.budget(),
            repetition: terminal.repetition(),
            timeout: terminal.timeout(),
        })
}

fn completed_progress_status(
    result: &NatsTurnResult,
    cancelled: bool,
    budget_exceeded: bool,
) -> SubAgentProgressStatus {
    if cancelled {
        SubAgentProgressStatus::Cancelled
    } else if budget_exceeded {
        SubAgentProgressStatus::Done
    } else if super::subagent_turn_failed(result, cancelled) {
        SubAgentProgressStatus::Failed
    } else {
        SubAgentProgressStatus::Done
    }
}

async fn finish_progress(
    reporter: &super::SubagentProgressReporter,
    status: SubAgentProgressStatus,
) -> Result<SubAgentProgress, ToolInvokeError> {
    reporter.finish(status).await.map_err(|error| {
        ToolInvokeError::Recoverable(format!("publish terminal sub-agent progress: {error:#}"))
    })
}

async fn load_public_progress(
    toolset: &SubagentToolset,
    result: &NatsTurnResult,
) -> Option<crate::PublicProgress> {
    let storage =
        harnx_core::session_identity::session_key(Some(&toolset.agent), &result.session_id);
    let log = crate::nats_session_log::NatsSessionLog::new(toolset.jetstream.clone(), storage);
    // Timeout receipt must not wait for optional progress reads or old conversation history.
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        log.load_events_after_async(result.user_msg_seq),
    )
    .await
    {
        Ok(Ok(entries)) => Some(crate::PublicProgress::from_entries(
            &entries,
            result.user_msg_seq,
        )),
        Ok(Err(error)) => {
            log::warn!("public progress unavailable: {error:#}");
            None
        }
        Err(_) => {
            log::warn!("public progress read exceeded 2s; progress unavailable");
            None
        }
    }
}

struct TerminationSpec {
    timeout: Option<crate::TimeoutTerminal>,
    kind: TerminationKind,
    budget: Option<u64>,
    repetition: Option<harnx_core::loop_guard::RepetitionTerminal>,
}

fn synthesize_termination(
    spec: TerminationSpec,
    session_id: &str,
    progress: &SubAgentProgress,
    buffering_sink: &InvocationBufferingSink,
    durable_progress: Option<&crate::PublicProgress>,
) -> SynthesizedResult {
    let thinking_tail = buffering_sink.thinking_tail();
    let public_progress = durable_progress
        .cloned()
        .filter(|p| p.available)
        .unwrap_or_else(|| buffering_sink.public_progress());
    synthesize_terminated_result(TerminationInputs {
        timeout: spec.timeout,
        public_progress: Some(&public_progress),
        kind: spec.kind,
        session_id,
        usage: &progress.usage,
        thinking_excerpt: Some(&thinking_tail),
        budget: spec.budget,
        repetition: spec.repetition,
    })
}

pub(super) fn result_value(
    toolset: &SubagentToolset,
    completed: &CompletedSubagentTurn,
) -> Result<serde_json::Value, ToolInvokeError> {
    let response = match &completed.termination {
        Some(termination) => termination.response.as_str(),
        None => super::require_response(
            completed
                .result
                .as_ref()
                .expect("completed sub-agent turn without result or termination"),
        )?,
    };
    let source = child_source(toolset, &completed.session_id);
    // The parent knows which of its calls this answers; the summary the model
    // reads keeps to the invocation's own facts.
    let progress = SubAgentProgress {
        tool_call_id: None,
        ..completed.progress.clone()
    };
    let mut value = serde_json::json!({
        "session_id": completed.session_id,
        "response": response,
        "sub_agent": source,
        "sub_agent_progress": progress,
    });
    if let Some(termination) = &completed.termination {
        value["termination"] = termination.termination_json();
    }
    Ok(value)
}

enum PromptTurn {
    Completed(Result<NatsTurnResult, ToolInvokeError>),
    Aborted(ToolInvokeError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recoverable_subagent_errors_include_session_id_without_tool_guidance() {
        let child_session_id = "child-error-session";
        assert_eq!(
            subagent_error_message("sub-agent turn was cancelled", child_session_id),
            "sub-agent turn was cancelled (session_id: child-error-session)"
        );

        for (worker_error, expected_prefix) in [
            (
                Some("worker failed"),
                "sub-agent turn failed: worker failed",
            ),
            (None, "sub-agent turn returned no final response"),
        ] {
            let result = NatsTurnResult {
                response: None,
                session_id: child_session_id.to_string(),
                was_cancelled: false,
                error: worker_error.map(str::to_string),
                user_msg_seq: 1,
                user_msg_id: "user-message".to_string(),
            };
            let error = super::super::require_response(&result).unwrap_err();
            let ToolInvokeError::Recoverable(message) = error else {
                panic!("expected recoverable tool error");
            };

            assert!(message.contains(expected_prefix));
            assert!(message.contains(child_session_id));
            assert!(!message.contains("session_prompt"));
            assert!(!message.contains("session_load"));
        }
    }

    fn turn_with_error(error: &str) -> NatsTurnResult {
        NatsTurnResult {
            response: None,
            session_id: "child".to_string(),
            was_cancelled: false,
            error: Some(error.to_string()),
            user_msg_seq: 1,
            user_msg_id: "user-message".to_string(),
        }
    }

    #[test]
    fn repetition_stop_becomes_a_failed_repetition_termination() {
        let stop = harnx_core::loop_guard::RepetitionStop(
            harnx_core::loop_guard::RepetitionTerminal::tool_calls("fs_read", 4),
        );
        let result = turn_with_error(&format!("worker turn: {stop}"));
        let spec = completed_termination_spec(&result).expect("repetition spec");
        assert_eq!(spec.kind, TerminationKind::Repetition);
        assert_eq!(spec.budget, None);
        assert_eq!(
            spec.repetition.as_ref().and_then(|t| t.tool.as_deref()),
            Some("fs_read")
        );
        assert_eq!(
            completed_progress_status(&result, false, false),
            SubAgentProgressStatus::Failed
        );
    }

    #[test]
    fn budget_and_plain_errors_keep_their_behaviour() {
        let budget =
            completed_termination_spec(&turn_with_error(&crate::budget_terminal_message(21, 20)))
                .expect("budget spec");
        assert_eq!(budget.kind, TerminationKind::BudgetExceeded);
        assert_eq!(budget.budget, Some(20));
        assert!(budget.repetition.is_none());
        assert!(completed_termination_spec(&turn_with_error("boom")).is_none());
    }
}
