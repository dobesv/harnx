use super::{CompletedSubagentTurn, ProgressReporterStart, SubagentToolset};
use crate::nats_session::{NatsSession, NatsTurnResult};
use crate::{
    parse_budget_terminal, synthesize_terminated_result, InvocationBufferingSink, RunTurnOptions,
    SynthesizedResult, TerminationInputs, TerminationKind,
};
use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};
use harnx_toolset::ToolInvokeError;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "expired_replay_tests.rs"]
mod expired_replay_tests;

pub(super) fn subagent_error_message(prefix: impl std::fmt::Display, session_id: &str) -> String {
    format!("{prefix} (session_id: {session_id})")
}

pub(super) struct PromptParams<'a> {
    pub message: &'a str,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub timeout_secs: Option<u64>,
    pub token_budget: Option<u64>,
    pub cancel: CancellationToken,
    pub context: harnx_toolset::ToolInvocationContext,
}

pub(super) async fn run_prompt(
    toolset: &SubagentToolset,
    params: PromptParams<'_>,
) -> Result<CompletedSubagentTurn, ToolInvokeError> {
    if params.cancel.is_cancelled() {
        return Err(ToolInvokeError::Fatal("sub-agent tool call aborted".into()));
    }
    let cancel = params.cancel.clone();
    let (session, deadline) = checkpointed_session(toolset, &params).await?;
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
        .await?;
    let buffering_sink = Arc::new(InvocationBufferingSink::new(reporter.sink()));
    let turn = await_prompt_turn(
        toolset,
        &session,
        &buffering_sink,
        AwaitTurnParams {
            message: params.message,
            timeout: deadline
                .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now())),
            token_budget: params.token_budget,
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
        PromptTurn::TimedOut(cancellation) => {
            finish_timed_out_turn(child_session_id, &reporter, &buffering_sink, cancellation).await
        }
    }
}

async fn checkpointed_session(
    toolset: &SubagentToolset,
    params: &PromptParams<'_>,
) -> Result<(NatsSession, Option<tokio::time::Instant>), ToolInvokeError> {
    let deadline = remaining_timeout(params.timeout_secs, None)
        .map(|remaining| tokio::time::Instant::now() + remaining);
    let parent = params.parent_session_id.as_deref();
    let session_id = params
        .context
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint["session_id"].as_str())
        .map(str::to_string)
        .or_else(|| params.session_id.clone());
    let session = toolset
        .create_session(session_id, parent, params.tool_call_id.as_deref())
        .await?;
    let Some(parent) = parent else {
        return Ok((session, deadline));
    };
    let Some(store) = params.context.checkpoint_store.as_ref() else {
        return Ok((session, deadline));
    };
    // First writer wins: a concurrent or replayed attempt converges on
    // whichever child id landed first, instead of running a second one.
    let stored = store
        .checkpoint(serde_json::json!({"session_id": session.session_id()}))
        .await
        .map_err(|error| {
            ToolInvokeError::Fatal(format!("persist sub-agent checkpoint: {error:#}"))
        })?;
    let id = stored["session_id"]
        .as_str()
        .ok_or_else(|| ToolInvokeError::Fatal("invalid sub-agent checkpoint".into()))?;
    if id == session.session_id() {
        return Ok((session, deadline));
    }
    Ok((
        toolset
            .create_session(
                Some(id.into()),
                Some(parent),
                params.tool_call_id.as_deref(),
            )
            .await?,
        deadline,
    ))
}

fn remaining_timeout(seconds: Option<u64>, started_at_ms: Option<u64>) -> Option<Duration> {
    seconds.filter(|seconds| *seconds > 0).map(|seconds| {
        let elapsed = started_at_ms.map_or(Duration::ZERO, |started| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH + Duration::from_millis(started))
                .unwrap_or_default()
        });
        Duration::from_secs(seconds).saturating_sub(elapsed)
    })
}

struct AwaitTurnParams<'a> {
    message: &'a str,
    timeout: Option<Duration>,
    token_budget: Option<u64>,
    cancel: CancellationToken,
}

async fn await_prompt_turn(
    _toolset: &SubagentToolset,
    session: &NatsSession,
    buffering_sink: &Arc<InvocationBufferingSink>,
    params: AwaitTurnParams<'_>,
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
    let (cancel_tx, cancel_rx) = mpsc::channel(1);
    let child = session.clone();
    let message = params.message.to_owned();
    let sink = buffering_sink.clone();
    let run_turn = tokio::spawn(async move {
        child
            .run_turn_with_options(
                &message,
                sink,
                Some(cancel_rx),
                RunTurnOptions {
                    token_budget: params.token_budget.filter(|budget| *budget > 0),
                },
            )
            .await
    });
    await_owned_turn(session, run_turn, cancel_tx, params).await
}

async fn await_owned_turn(
    session: &NatsSession,
    mut run_turn: tokio::task::JoinHandle<anyhow::Result<NatsTurnResult>>,
    cancel_tx: mpsc::Sender<()>,
    params: AwaitTurnParams<'_>,
) -> PromptTurn {
    let deadline = invocation_deadline(params.timeout);
    tokio::pin!(deadline);

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
            let _ = ensure_timeout_cancellation(session).await;
            PromptTurn::Aborted(ToolInvokeError::Fatal(
                "sub-agent tool call aborted".to_string(),
            ))
        }
        _ = &mut deadline => {
            let _ = cancel_tx.try_send(());
            supervise_turn(run_turn);
            PromptTurn::TimedOut(ensure_timeout_cancellation(session).await)
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

async fn invocation_deadline(timeout: Option<Duration>) {
    match timeout {
        Some(duration) => tokio::time::sleep(duration).await,
        None => std::future::pending::<()>().await,
    }
}

async fn ensure_timeout_cancellation(session: &NatsSession) -> Result<(), ToolInvokeError> {
    let session_id = session.session_id();
    session
        .interrupt("parent interrupted")
        .await
        .map_err(|error| {
            timeout_cancellation_error(
                session_id,
                format!("cancellation request failed: {error:#}"),
            )
        })?;
    Ok(())
}

fn timeout_cancellation_error(session_id: &str, reason: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!(
        "sub-agent timeout: durable cancellation could not be confirmed for session '{session_id}'; not safe to retry: {reason}"
    ))
}

async fn finish_timed_out_turn(
    session_id: String,
    reporter: &super::SubagentProgressReporter,
    buffering_sink: &InvocationBufferingSink,
    cancellation: Result<(), ToolInvokeError>,
) -> Result<CompletedSubagentTurn, ToolInvokeError> {
    // A timeout promises logical stop, not physical termination. The worker
    // releases execution independently; a late remote side effect cannot be undone.
    let status = if cancellation.is_ok() {
        SubAgentProgressStatus::Cancelled
    } else {
        SubAgentProgressStatus::Unconfirmed
    };
    let progress = finish_progress(reporter, status).await;
    if let Err(error) = cancellation {
        if let Err(report_error) = progress {
            log::debug!("failed to publish terminal sub-agent progress: {report_error:#}");
        }
        return Err(error);
    }
    let progress = progress?;
    let termination = synthesize_termination(
        TerminationSpec {
            kind: TerminationKind::Timeout,
            budget: None,
        },
        &session_id,
        &progress,
        buffering_sink,
    );
    Ok(CompletedSubagentTurn {
        session_id,
        result: None,
        progress,
        termination: Some(termination),
    })
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
    let budget_terminal = params
        .result
        .error
        .as_deref()
        .and_then(parse_budget_terminal);
    let status = completed_progress_status(&params.result, cancelled, budget_terminal.is_some());
    let progress = finish_progress(&params.reporter, status).await?;
    if cancelled {
        return Err(ToolInvokeError::Recoverable(subagent_error_message(
            "sub-agent turn was cancelled",
            &params.child_session_id,
        )));
    }
    let termination = budget_terminal.map(|terminal| {
        synthesize_termination(
            TerminationSpec {
                kind: TerminationKind::BudgetExceeded,
                budget: Some(terminal.budget),
            },
            &params.child_session_id,
            &progress,
            &params.buffering_sink,
        )
    });
    Ok(CompletedSubagentTurn {
        session_id: params.child_session_id,
        result: Some(params.result),
        progress,
        termination,
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

struct TerminationSpec {
    kind: TerminationKind,
    budget: Option<u64>,
}
fn synthesize_termination(
    spec: TerminationSpec,
    session_id: &str,
    progress: &SubAgentProgress,
    buffering_sink: &InvocationBufferingSink,
) -> SynthesizedResult {
    let thinking_tail = buffering_sink.thinking_tail();
    synthesize_terminated_result(TerminationInputs {
        kind: spec.kind,
        session_id,
        usage: &progress.usage,
        thinking_excerpt: Some(&thinking_tail),
        budget: spec.budget,
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
    let source = harnx_core::event::AgentSource {
        agent: toolset.agent.clone(),
        session_id: Some(completed.session_id.clone()),
        model: None,
    };
    let mut value = serde_json::json!({
        "session_id": completed.session_id,
        "response": response,
        "sub_agent": source,
        "sub_agent_progress": completed.progress,
    });
    if let Some(termination) = &completed.termination {
        value["termination"] = termination.termination_json();
    }
    Ok(value)
}

enum PromptTurn {
    Completed(Result<NatsTurnResult, ToolInvokeError>),
    Aborted(ToolInvokeError),
    TimedOut(Result<(), ToolInvokeError>),
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

    #[tokio::test]
    async fn timeout_cancellation_failure_is_recoverable_and_finishes_reporter() {
        let reporter = super::super::SubagentProgressReporter::spawn(
            "helper".to_string(),
            "unsafe-session".to_string(),
            "invocation".to_string(),
            None,
            Duration::from_secs(60),
        );
        let buffering_sink = InvocationBufferingSink::new(reporter.sink());
        let cancellation = Err(timeout_cancellation_error(
            "unsafe-session",
            "durable cancellation request failed",
        ));

        let error = match finish_timed_out_turn(
            "unsafe-session".to_string(),
            &reporter,
            &buffering_sink,
            cancellation,
        )
        .await
        {
            Ok(_) => panic!("unsafe timeout must not return a synthesized retry result"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            ToolInvokeError::Recoverable(
                "sub-agent timeout: durable cancellation could not be confirmed for session 'unsafe-session'; not safe to retry: durable cancellation request failed".to_string()
            )
        );
        let second_finish = tokio::time::timeout(
            Duration::from_secs(1),
            reporter.finish(SubAgentProgressStatus::Done),
        )
        .await
        .expect("reporter completion check timed out");
        assert!(second_finish.is_err(), "reporter must already be finished");
    }
}
