use super::{CompletedSubagentTurn, SubagentToolset};
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

const CANCEL_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
const CANCEL_RELEASE_POLL: Duration = Duration::from_millis(10);

pub(super) struct PromptParams<'a> {
    pub message: &'a str,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub timeout_secs: Option<u64>,
    pub token_budget: Option<u64>,
    pub cancel: CancellationToken,
}

pub(super) async fn run_prompt(
    toolset: &SubagentToolset,
    params: PromptParams<'_>,
) -> Result<CompletedSubagentTurn, ToolInvokeError> {
    let session = toolset.create_session(params.session_id).await?;
    let child_session_id = session.session_id().to_string();
    let reporter = toolset
        .start_progress_reporter(&child_session_id, params.parent_session_id)
        .await?;
    let buffering_sink = Arc::new(InvocationBufferingSink::new(reporter.sink()));
    let turn = await_prompt_turn(
        toolset,
        &session,
        &buffering_sink,
        AwaitTurnParams {
            message: params.message,
            timeout_secs: params.timeout_secs,
            token_budget: params.token_budget,
            cancel: params.cancel,
        },
    )
    .await;

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

struct AwaitTurnParams<'a> {
    message: &'a str,
    timeout_secs: Option<u64>,
    token_budget: Option<u64>,
    cancel: CancellationToken,
}

async fn await_prompt_turn(
    toolset: &SubagentToolset,
    session: &NatsSession,
    buffering_sink: &Arc<InvocationBufferingSink>,
    params: AwaitTurnParams<'_>,
) -> PromptTurn {
    let (cancel_tx, cancel_rx) = mpsc::channel(1);
    let run_turn = session.run_turn_with_options(
        params.message,
        buffering_sink.clone(),
        Some(cancel_rx),
        RunTurnOptions {
            token_budget: params.token_budget.filter(|budget| *budget > 0),
        },
    );
    tokio::pin!(run_turn);
    let deadline = invocation_deadline(params.timeout_secs);
    tokio::pin!(deadline);

    let turn = tokio::select! {
        result = &mut run_turn => PromptTurn::Completed(result.map_err(|error| {
            ToolInvokeError::Recoverable(format!("run sub-agent turn: {error:#}"))
        })),
        _ = params.cancel.cancelled() => {
            let _ = cancel_tx.send(()).await;
            let _ = (&mut run_turn).await;
            PromptTurn::Aborted(ToolInvokeError::Fatal(
                "sub-agent tool call aborted".to_string(),
            ))
        }
        _ = &mut deadline => {
            let _ = cancel_tx.send(()).await;
            let _ = (&mut run_turn).await;
            PromptTurn::TimedOut(ensure_timeout_cancellation(toolset, session).await)
        }
    };

    turn
}

async fn invocation_deadline(timeout_secs: Option<u64>) {
    match timeout_secs.filter(|seconds| *seconds > 0) {
        Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
        None => std::future::pending::<()>().await,
    }
}

async fn ensure_timeout_cancellation(
    toolset: &SubagentToolset,
    session: &NatsSession,
) -> Result<(), ToolInvokeError> {
    let session_id = session.session_id();
    session.cancel_pending_turn().await.map_err(|error| {
        timeout_cancellation_error(
            session_id,
            format!("cancellation request failed: {error:#}"),
        )
    })?;
    wait_for_session_lease_release(toolset, session_id)
        .await
        .map_err(|error| timeout_cancellation_error(session_id, error))
}

fn timeout_cancellation_error(session_id: &str, reason: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!(
        "sub-agent timeout: durable cancellation could not be confirmed for session '{session_id}'; not safe to retry: {reason}"
    ))
}

async fn wait_for_session_lease_release(
    toolset: &SubagentToolset,
    session_id: &str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + CANCEL_RELEASE_TIMEOUT;
    loop {
        match crate::nats_lease::session_has_active_lease(&toolset.jetstream, session_id).await {
            Ok(false) => return Ok(()),
            Ok(true) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(CANCEL_RELEASE_POLL).await;
            }
            Ok(true) => {
                return Err(format!(
                    "session lease was still active after {} seconds",
                    CANCEL_RELEASE_TIMEOUT.as_secs()
                ));
            }
            Err(error) => return Err(format!("session lease check failed: {error:#}")),
        }
    }
}

async fn finish_timed_out_turn(
    session_id: String,
    reporter: &super::SubagentProgressReporter,
    buffering_sink: &InvocationBufferingSink,
    cancellation: Result<(), ToolInvokeError>,
) -> Result<CompletedSubagentTurn, ToolInvokeError> {
    // A timeout result promises same-session retry. Finish reporting first, but
    // don't emit that result unless durable cancellation and lease release were confirmed.
    let status = if cancellation.is_ok() {
        SubAgentProgressStatus::Done
    } else {
        SubAgentProgressStatus::Failed
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
        return Err(ToolInvokeError::Recoverable(
            "sub-agent turn was cancelled".to_string(),
        ));
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
    if budget_exceeded {
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
