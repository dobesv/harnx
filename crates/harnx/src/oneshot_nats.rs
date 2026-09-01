use harnx_core::abort::AbortSignal;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent};
use harnx_runtime::{
    parse_budget_terminal, synthesize_terminated_result, InvocationBufferingSink, NatsSession,
    NatsTurnResult, RunTurnOptions, SynthesizedResult, TerminationInputs, TerminationKind,
};
use parking_lot::Mutex;
use std::{
    fmt,
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub(crate) const INVOCATION_LIMIT_EXIT_CODE: i32 = 2;

#[derive(Debug)]
pub(crate) struct InvocationLimitReached;

impl fmt::Display for InvocationLimitReached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("one-shot invocation limit reached")
    }
}

impl std::error::Error for InvocationLimitReached {}

pub(crate) struct InvocationOptions {
    abort_signal: AbortSignal,
    final_only: bool,
    timeout_secs: Option<u64>,
    token_budget: Option<u64>,
}

impl InvocationOptions {
    pub(crate) fn new(
        abort_signal: AbortSignal,
        final_only: bool,
        timeout_secs: Option<u64>,
        token_budget: Option<u64>,
    ) -> Self {
        Self {
            abort_signal,
            final_only,
            timeout_secs: timeout_secs.filter(|seconds| *seconds > 0),
            token_budget: token_budget.filter(|budget| *budget > 0),
        }
    }

    pub(crate) fn abort_signal(&self) -> &AbortSignal {
        &self.abort_signal
    }

    pub(crate) fn final_only(&self) -> bool {
        self.final_only
    }
}

pub(crate) struct AssistantTextTrackingSink {
    inner: Arc<dyn AgentEventSink>,
    rendered_assistant_text: AtomicBool,
    usage: Mutex<CompletionTokenUsage>,
}

impl AssistantTextTrackingSink {
    pub(crate) fn new(inner: Arc<dyn AgentEventSink>) -> Self {
        Self {
            inner,
            rendered_assistant_text: AtomicBool::new(false),
            usage: Mutex::new(CompletionTokenUsage::default()),
        }
    }

    pub(crate) fn observed_usage(&self) -> CompletionTokenUsage {
        self.usage.lock().clone()
    }

    pub(crate) fn emit_durable_response_if_needed(&self, result: NatsTurnResult) {
        if result.was_cancelled || self.rendered_assistant_text.load(Ordering::Acquire) {
            return;
        }
        if let Some(response) = result.response.filter(|response| !response.is_empty()) {
            self.emit(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(response)],
            }));
        }
    }
}

impl AgentEventSink for AssistantTextTrackingSink {
    fn emit(&self, event: AgentEvent) {
        if event_has_assistant_text(&event) {
            self.rendered_assistant_text.store(true, Ordering::Release);
        }
        if let AgentEvent::Model(ModelEvent::Usage {
            input,
            output,
            cached,
            cache_write,
            ..
        }) = &event
        {
            self.usage.lock().accumulate(&CompletionTokenUsage {
                input_tokens: *input,
                output_tokens: *output,
                cached_tokens: *cached,
                cache_write_tokens: *cache_write,
            });
        }
        self.inner.emit(event);
    }
}

fn event_has_assistant_text(event: &AgentEvent) -> bool {
    match event {
        AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if !text.is_empty())),
        AgentEvent::SubAgent { event, .. } => event_has_assistant_text(event),
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TerminationOutput {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) fn termination_output(
    synthesized: &SynthesizedResult,
) -> anyhow::Result<TerminationOutput> {
    let mut stdout = synthesized.response.clone();
    if !stdout.ends_with('\n') {
        stdout.push('\n');
    }
    let mut stderr = serde_json::to_string(&synthesized.termination_json())?;
    stderr.push('\n');
    Ok(TerminationOutput { stdout, stderr })
}

pub(crate) fn emit_termination(synthesized: &SynthesizedResult) -> anyhow::Result<()> {
    let output = termination_output(synthesized)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(output.stdout.as_bytes())?;
    stdout.flush()?;
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(output.stderr.as_bytes())?;
    stderr.flush()?;
    Ok(())
}

pub(crate) async fn run_turn(
    session: &NatsSession,
    input_text: &str,
    tracking_sink: Arc<AssistantTextTrackingSink>,
    options: &InvocationOptions,
) -> anyhow::Result<Option<NatsTurnResult>> {
    let run_turn = session.run_turn_with_options(
        input_text,
        tracking_sink,
        None,
        RunTurnOptions {
            token_budget: options.token_budget,
        },
    );
    tokio::pin!(run_turn);
    let deadline = async {
        match options.timeout_secs {
            Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);

    tokio::select! {
        result = &mut run_turn => Ok(Some(result?)),
        _ = &mut deadline => {
            // Timeout is caller-local: only this deadline arm classifies a timeout.
            options.abort_signal.set_ctrlc();
            if let Err(error) = (&mut run_turn).await {
                log::debug!(
                    "one-shot turn cleanup failed after timeout for session '{}': {error:#}",
                    session.session_id(),
                );
            }
            finish_timed_out_turn(session.session_id(), session.cancel_pending_turn().await)
        }
    }
}

fn finish_timed_out_turn(
    session_id: &str,
    cancellation: anyhow::Result<bool>,
) -> anyhow::Result<Option<NatsTurnResult>> {
    // Synthesized timeout output promises same-session retry, so cancellation
    // confirmation failure is an infrastructure error, not an invocation limit.
    cancellation.map(|_| None).map_err(|error| {
        anyhow::anyhow!(
            "one-shot timeout: durable cancellation could not be confirmed for session '{session_id}'; not safe to retry: {error:#}"
        )
    })
}

pub(crate) struct TurnOutput<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) buffering_sink: &'a InvocationBufferingSink,
    pub(crate) tracking_sink: &'a AssistantTextTrackingSink,
    pub(crate) options: &'a InvocationOptions,
}

#[derive(Debug, PartialEq, Eq)]
struct TerminationSpec {
    kind: TerminationKind,
    budget: Option<u64>,
}

fn termination_spec(result: Option<&NatsTurnResult>) -> Option<TerminationSpec> {
    match result {
        None => Some(TerminationSpec {
            kind: TerminationKind::Timeout,
            budget: None,
        }),
        Some(result) => result
            .error
            .as_deref()
            .and_then(parse_budget_terminal)
            .map(|terminal| TerminationSpec {
                kind: TerminationKind::BudgetExceeded,
                budget: Some(terminal.budget),
            }),
    }
}
pub(crate) fn finish_turn(
    result: Option<NatsTurnResult>,
    output: TurnOutput<'_>,
) -> anyhow::Result<()> {
    if let Some(termination) = termination_spec(result.as_ref()) {
        let thinking_tail = output.buffering_sink.thinking_tail();
        let usage = output.tracking_sink.observed_usage();
        let synthesized = synthesize_terminated_result(TerminationInputs {
            kind: termination.kind,
            session_id: output.session_id,
            usage: &usage,
            thinking_excerpt: Some(&thinking_tail),
            budget: termination.budget,
        });
        emit_termination(&synthesized)?;
        return Err(InvocationLimitReached.into());
    }

    let result = result.ok_or_else(|| anyhow::anyhow!("one-shot completion returned no result"))?;
    let worker_error = result.error.clone();
    if output.options.final_only {
        print_final_response(&result);
    } else {
        output.tracking_sink.emit_durable_response_if_needed(result);
    }
    match worker_error {
        Some(error) => Err(anyhow::anyhow!(error)),
        None => Ok(()),
    }
}

fn print_final_response(result: &NatsTurnResult) {
    if result.was_cancelled || result.error.is_some() {
        return;
    }
    let Some(response) = result.response.as_deref().filter(|text| !text.is_empty()) else {
        return;
    };
    print!("{response}");
    if !response.ends_with('\n') {
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::AgentSource;

    fn sample_termination_usage() -> CompletionTokenUsage {
        CompletionTokenUsage {
            input_tokens: 20,
            output_tokens: 5,
            cached_tokens: 4,
            cache_write_tokens: 2,
        }
    }

    #[test]
    fn timeout_cancellation_failure_is_generic_error_not_invocation_limit() {
        let result = finish_timed_out_turn(
            "unsafe-session",
            Err(anyhow::anyhow!("injected cancellation failure")),
        );

        let error = result.expect_err("unconfirmed cancellation must fail the invocation");
        assert_eq!(
            error.to_string(),
            "one-shot timeout: durable cancellation could not be confirmed for session 'unsafe-session'; not safe to retry: injected cancellation failure"
        );
        assert!(!error.is::<InvocationLimitReached>());
        assert!(!crate::invocation_limit_reached(&error));
    }

    #[test]
    fn timeout_cancellation_success_is_synthesized_invocation_limit() {
        let result = finish_timed_out_turn("safe-session", Ok(true))
            .expect("confirmed cancellation must preserve timeout classification");

        assert!(result.is_none());
        assert_eq!(
            termination_spec(result.as_ref()),
            Some(TerminationSpec {
                kind: TerminationKind::Timeout,
                budget: None,
            })
        );
        let marker = anyhow::Error::from(InvocationLimitReached);
        assert!(crate::invocation_limit_reached(&marker));
        assert_eq!(INVOCATION_LIMIT_EXIT_CODE, 2);
    }

    #[test]
    fn timeout_output_has_synthesized_stdout_single_json_stderr_line_and_exit_code_two() {
        let usage = sample_termination_usage();
        let synthesized = synthesize_terminated_result(TerminationInputs {
            kind: TerminationKind::Timeout,
            session_id: "cli-timeout-session",
            usage: &usage,
            thinking_excerpt: Some("partial thought"),
            budget: None,
        });

        let output = termination_output(&synthesized).unwrap();
        assert_eq!(output.stdout, format!("{}\n", synthesized.response));
        assert!(output.stdout.contains("reaching its time limit"));
        assert_eq!(output.stderr.lines().count(), 1);
        let stderr_json: serde_json::Value = serde_json::from_str(output.stderr.trim()).unwrap();
        assert_eq!(stderr_json["kind"], "timeout");
        assert_eq!(stderr_json["session_id"], "cli-timeout-session");
        let marker = anyhow::Error::from(InvocationLimitReached);
        assert!(marker.is::<InvocationLimitReached>());
        assert_eq!(INVOCATION_LIMIT_EXIT_CODE, 2);
    }

    #[test]
    fn parsed_budget_terminal_has_synthesized_stdout_and_single_json_stderr_line() {
        let turn = NatsTurnResult {
            response: None,
            session_id: "cli-budget-session".to_string(),
            was_cancelled: false,
            error: Some(harnx_runtime::budget_terminal_message(21, 20)),
            user_msg_seq: 1,
            user_msg_id: "user-message".to_string(),
        };
        let termination = termination_spec(Some(&turn)).expect("budget termination");
        assert_eq!(
            termination,
            TerminationSpec {
                kind: TerminationKind::BudgetExceeded,
                budget: Some(20),
            }
        );
        let usage = sample_termination_usage();
        let synthesized = synthesize_terminated_result(TerminationInputs {
            kind: termination.kind,
            session_id: "cli-budget-session",
            usage: &usage,
            thinking_excerpt: None,
            budget: termination.budget,
        });

        let output = termination_output(&synthesized).unwrap();
        assert!(output.stdout.contains("reached its token budget"));
        assert!(output
            .stdout
            .contains("same session id `cli-budget-session`"));
        assert_eq!(output.stderr.lines().count(), 1);
        let stderr_json: serde_json::Value = serde_json::from_str(output.stderr.trim()).unwrap();
        assert_eq!(stderr_json["kind"], "budget_exceeded");
        assert_eq!(stderr_json["session_id"], "cli-budget-session");
    }

    fn usage_event(input: u64, output: u64, cached: u64, cache_write: u64) -> AgentEvent {
        AgentEvent::Model(ModelEvent::Usage {
            input,
            output,
            cached,
            cache_write,
            session_label: None,
        })
    }

    #[test]
    fn tracks_direct_usage_and_excludes_nested_invocations() {
        let sink = AssistantTextTrackingSink::new(Arc::new(harnx_core::event::NullSink));
        sink.emit(usage_event(10, 3, 4, 2));
        sink.emit(usage_event(5, 2, 1, 0));
        sink.emit(AgentEvent::sub_agent(
            AgentSource {
                agent: "nested".into(),
                session_id: Some("nested-session".into()),
                model: None,
            },
            usage_event(100, 100, 0, 0),
        ));

        assert_eq!(
            sink.observed_usage(),
            CompletionTokenUsage {
                input_tokens: 15,
                output_tokens: 5,
                cached_tokens: 5,
                cache_write_tokens: 2,
            }
        );
    }
}
