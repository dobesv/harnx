use anyhow::Context;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent};
use harnx_runtime::nats_session::InterruptOutcome;
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

/// Callback type for tool timer tick notifications.
/// Prints "still running" notices directly under lock.
pub(crate) type ToolTimerTickFn = Arc<dyn Fn() + Send + Sync>;

/// Callback type for clearing tool timers on exit.
pub(crate) type ClearToolTimersFn = Arc<dyn Fn() + Send + Sync>;

pub(crate) const INVOCATION_LIMIT_EXIT_CODE: i32 = 2;

/// Outcome of the select loop race — which arm won.
/// Used to ensure side effects (interrupt calls) happen after the select!
/// completes, avoiding race conditions with biased polling.
#[derive(Debug)]
pub(crate) enum TurnLoopOutcome<T> {
    /// User abort signal fired.
    Aborted,
    /// Turn completed with a result.
    Completed(T),
    /// Deadline elapsed.
    TimedOut,
}

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

/// Runs the tokio::select! loop that drives a turn with abort/timeout/ticker arms.
///
/// This is extracted from `run_turn` to allow direct testing of the select! loop
/// without requiring a real NatsSession.
///
/// Returns a `TurnLoopOutcome` indicating which arm won the race. Side effects
/// (like calling `session.interrupt`) must NOT be executed inside the select!
/// arms to avoid race conditions with biased polling. Instead, callers should
/// match on the outcome and perform side effects after the select! completes.
pub(crate) async fn run_turn_select_loop<F, FutAbort, FutDeadline>(
    mut run_turn: F,
    mut abort_signal: FutAbort,
    mut deadline: FutDeadline,
    tool_timer_tick: Option<ToolTimerTickFn>,
    clear_tool_timers: ClearToolTimersFn,
) -> TurnLoopOutcome<anyhow::Result<NatsTurnResult>>
where
    F: std::future::Future<Output = anyhow::Result<NatsTurnResult>> + Unpin,
    FutAbort: std::future::Future<Output = ()> + Unpin,
    FutDeadline: std::future::Future<Output = ()> + Unpin,
{
    // RAII guard ensures clear_tool_timers is called on every exit path.
    let _guard = scopeguard::guard(clear_tool_timers, |clear| clear());

    // 1s ticker for tool call "still running" notices.
    // Only active when final_only is false (normal human-readable output mode).
    let mut ticker = if tool_timer_tick.is_some() {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Some(interval)
    } else {
        None
    };

    tokio::select! {
        biased;

        _ = &mut abort_signal => TurnLoopOutcome::Aborted,
        res = &mut run_turn => TurnLoopOutcome::Completed(res),
        _ = &mut deadline => TurnLoopOutcome::TimedOut,
        _ = async {
            if let Some(ref mut interval) = ticker {
                loop {
                    interval.tick().await;
                    if let Some(ref tick_fn) = tool_timer_tick {
                        tick_fn();
                    }
                }
            } else {
                std::future::pending::<()>().await;
            }
        }, if ticker.is_some() => {
            unreachable!("ticker arm should never complete")
        }
    }
}

pub(crate) async fn run_turn(
    session: &NatsSession,
    input_text: &str,
    tracking_sink: Arc<AssistantTextTrackingSink>,
    options: &InvocationOptions,
    tool_timer_tick: Option<ToolTimerTickFn>,
    clear_tool_timers: ClearToolTimersFn,
) -> anyhow::Result<Option<NatsTurnResult>> {
    let run_turn = session.run_turn_with_options(
        input_text,
        tracking_sink,
        None,
        RunTurnOptions {
            token_budget: options.token_budget,
            ..Default::default()
        },
    );
    tokio::pin!(run_turn);

    let abort_signal = wait_abort_signal(options.abort_signal());
    tokio::pin!(abort_signal);

    let deadline = async {
        match options.timeout_secs {
            Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);

    let outcome = run_turn_select_loop(
        run_turn,
        abort_signal,
        deadline,
        tool_timer_tick,
        clear_tool_timers,
    )
    .await;

    match outcome {
        TurnLoopOutcome::Aborted => {
            let outcome = session.interrupt("user interrupt from cli").await;
            eprintln!("{}", describe_interrupt_outcome(&outcome));
            outcome.with_context(|| {
                format!("failed to interrupt session '{}'", session.session_id())
            })?;
            Err(anyhow::anyhow!("interrupted by user"))
        }
        TurnLoopOutcome::Completed(res) => Ok(Some(res?)),
        TurnLoopOutcome::TimedOut => {
            let outcome = session.interrupt("one-shot timeout").await;
            if outcome.is_err() {
                eprintln!("{}", describe_interrupt_outcome(&outcome));
            }
            finish_timed_out_turn(session.session_id(), outcome)
        }
    }
}

/// One-line, human-readable summary of an interrupt outcome for stderr — the
/// only feedback the CLI gives before it exits that the append landed (or
/// didn't).
fn describe_interrupt_outcome(outcome: &anyhow::Result<InterruptOutcome>) -> String {
    match outcome {
        Ok(InterruptOutcome::Idle) => "no turn was running".to_string(),
        Ok(InterruptOutcome::Accepted { cancel_seq }) => {
            format!("interrupt accepted (cancel seq {cancel_seq})")
        }
        Ok(InterruptOutcome::AlreadyInterrupted { cancel_seq }) => {
            format!("already interrupted (cancel seq {cancel_seq})")
        }
        Err(error) => format!("interrupt could not be appended: {error:#}"),
    }
}

fn finish_timed_out_turn(
    session_id: &str,
    outcome: anyhow::Result<InterruptOutcome>,
) -> anyhow::Result<Option<NatsTurnResult>> {
    // Synthesized timeout output promises same-session retry, so a failed
    // append is an infrastructure error, not an invocation limit: every
    // confirmed outcome (Idle, Accepted or AlreadyInterrupted) preserves the
    // timeout classification, and only a failed append is not safe to retry.
    outcome.map(|_| None).map_err(|error| {
        anyhow::anyhow!(
            "one-shot timeout: interrupt could not be appended for session '{session_id}'; not safe to retry: {error:#}"
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
            Err(anyhow::anyhow!("injected append failure")),
        );

        let error = result.expect_err("a failed interrupt append must fail the invocation");
        assert_eq!(
            error.to_string(),
            "one-shot timeout: interrupt could not be appended for session 'unsafe-session'; not safe to retry: injected append failure"
        );
        assert!(!error.is::<InvocationLimitReached>());
        assert!(!crate::invocation_limit_reached(&error));
    }

    #[test]
    fn timeout_cancellation_success_is_synthesized_invocation_limit() {
        // Every confirmed outcome — a fresh Cancel, one that already landed,
        // or no turn to interrupt at all — preserves the timeout
        // classification; only a failed append (covered above) does not.
        for outcome in [
            InterruptOutcome::Accepted { cancel_seq: 7 },
            InterruptOutcome::AlreadyInterrupted { cancel_seq: 7 },
            InterruptOutcome::Idle,
        ] {
            let result = finish_timed_out_turn("safe-session", Ok(outcome))
                .expect("a confirmed interrupt outcome must preserve timeout classification");

            assert!(result.is_none());
            assert_eq!(
                termination_spec(result.as_ref()),
                Some(TerminationSpec {
                    kind: TerminationKind::Timeout,
                    budget: None,
                })
            );
        }
        let marker = anyhow::Error::from(InvocationLimitReached);
        assert!(crate::invocation_limit_reached(&marker));
        assert_eq!(INVOCATION_LIMIT_EXIT_CODE, 2);
    }

    #[test]
    fn describe_interrupt_outcome_summarizes_every_case_on_one_line() {
        assert_eq!(
            describe_interrupt_outcome(&Ok(InterruptOutcome::Idle)),
            "no turn was running"
        );
        assert_eq!(
            describe_interrupt_outcome(&Ok(InterruptOutcome::Accepted { cancel_seq: 9 })),
            "interrupt accepted (cancel seq 9)"
        );
        assert_eq!(
            describe_interrupt_outcome(&Ok(InterruptOutcome::AlreadyInterrupted { cancel_seq: 3 })),
            "already interrupted (cancel seq 3)"
        );
        let message = describe_interrupt_outcome(&Err(anyhow::anyhow!("nats unreachable")));
        assert!(
            message.contains("nats unreachable"),
            "expected the append error in the message: {message}"
        );
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

    /// Tests for `run_turn_select_loop` that call the production code directly.
    ///
    /// These tests verify the actual select! loop behavior including:
    /// - Ticker arm fires at 1-second intervals during turn execution.
    /// - Scopeguard cleanup runs on all exit paths (success, error, timeout, abort).
    /// - Cleanup runs exactly once per invocation.
    /// - Correct `TurnLoopOutcome` variant returned for each case.
    /// - Timeout always wins over abort when both are ready (biased select priority).
    /// Test that ticker fires 3 times during a 3-second turn and cleanup runs on success.
    /// Uses `start_paused` to control tokio time without manual pausing.
    #[tokio::test(start_paused = true)]
    async fn test_select_loop_ticker_fires_and_cleans_up_on_success() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tick_count = Arc::new(AtomicUsize::new(0));
        let clear_count = Arc::new(AtomicUsize::new(0));

        let tick_fn = {
            let tick_count = tick_count.clone();
            Arc::new(move || {
                tick_count.fetch_add(1, Ordering::SeqCst);
            }) as ToolTimerTickFn
        };

        let clear_fn = {
            let clear_count = clear_count.clone();
            Arc::new(move || {
                clear_count.fetch_add(1, Ordering::SeqCst);
            }) as ClearToolTimersFn
        };

        // Turn future: sleeps for 3 seconds then returns success
        let turn_future = Box::pin(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            Ok(NatsTurnResult {
                response: None,
                session_id: "test-session".to_string(),
                was_cancelled: false,
                error: None,
                user_msg_seq: 0,
                user_msg_id: "test-msg-id".to_string(),
            })
        });

        // Abort signal: never fires
        let abort_signal = std::future::pending::<()>();

        // Deadline: never fires
        let deadline = std::future::pending::<()>();

        let outcome =
            run_turn_select_loop(turn_future, abort_signal, deadline, Some(tick_fn), clear_fn)
                .await;

        assert!(
            matches!(outcome, TurnLoopOutcome::Completed(Ok(_))),
            "turn should complete successfully"
        );
        assert!(
            tick_count.load(Ordering::SeqCst) >= 3,
            "ticker should have fired at least 3 times during 3-second turn, got {}",
            tick_count.load(Ordering::SeqCst)
        );
        assert_eq!(
            clear_count.load(Ordering::SeqCst),
            1,
            "clear_tool_timers should run exactly once on normal exit"
        );
    }

    /// Test that cleanup runs when the turn future returns an error.
    #[tokio::test]
    async fn test_select_loop_cleans_up_on_turn_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let clear_count = Arc::new(AtomicUsize::new(0));

        let clear_fn = {
            let clear_count = clear_count.clone();
            Arc::new(move || {
                clear_count.fetch_add(1, Ordering::SeqCst);
            }) as ClearToolTimersFn
        };

        // Turn future: immediately returns error
        let turn_future = Box::pin(async { Err(anyhow::anyhow!("turn failed")) });

        // Abort signal: pending
        let abort_signal = std::future::pending::<()>();

        // Deadline: pending
        let deadline = std::future::pending::<()>();

        let outcome = run_turn_select_loop(
            turn_future,
            abort_signal,
            deadline,
            None, // No ticker needed for error path
            clear_fn,
        )
        .await;

        assert!(
            matches!(outcome, TurnLoopOutcome::Completed(Err(_))),
            "turn should return error inside Completed"
        );
        assert_eq!(
            clear_count.load(Ordering::SeqCst),
            1,
            "clear_tool_timers should run even on turn error"
        );
    }

    /// Test that cleanup runs when the timeout arm fires.
    #[tokio::test(start_paused = true)]
    async fn test_select_loop_cleans_up_on_timeout() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let clear_count = Arc::new(AtomicUsize::new(0));

        let clear_fn = {
            let clear_count = clear_count.clone();
            Arc::new(move || {
                clear_count.fetch_add(1, Ordering::SeqCst);
            }) as ClearToolTimersFn
        };

        // Turn future: pending (never completes)
        let turn_future = Box::pin(std::future::pending::<anyhow::Result<NatsTurnResult>>())
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<NatsTurnResult>> + Send>,
            >;

        // Abort signal: pending
        let abort_signal = Box::pin(std::future::pending::<()>())
            as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

        // Deadline: completes after 1 second
        let deadline = Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

        let outcome = run_turn_select_loop(
            turn_future,
            abort_signal,
            deadline,
            None, // No ticker needed for timeout path
            clear_fn,
        )
        .await;

        assert!(
            matches!(outcome, TurnLoopOutcome::TimedOut),
            "should return TimedOut"
        );
        assert_eq!(
            clear_count.load(Ordering::SeqCst),
            1,
            "clear_tool_timers should run on timeout"
        );
    }

    /// Test that cleanup runs when the abort arm fires.
    #[tokio::test(start_paused = true)]
    async fn test_select_loop_cleans_up_on_abort() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let clear_count = Arc::new(AtomicUsize::new(0));

        let clear_fn = {
            let clear_count = clear_count.clone();
            Arc::new(move || {
                clear_count.fetch_add(1, Ordering::SeqCst);
            }) as ClearToolTimersFn
        };

        // Turn future: pending (never completes)
        let turn_future = Box::pin(std::future::pending::<anyhow::Result<NatsTurnResult>>())
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<NatsTurnResult>> + Send>,
            >;

        // Abort signal: fires after 1 second
        let abort_signal = Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

        // Deadline: pending
        let deadline = Box::pin(std::future::pending::<()>())
            as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

        let outcome = run_turn_select_loop(
            turn_future,
            abort_signal,
            deadline,
            None, // No ticker needed for abort path
            clear_fn,
        )
        .await;

        assert!(
            matches!(outcome, TurnLoopOutcome::Aborted),
            "should return Aborted"
        );
        assert_eq!(
            clear_count.load(Ordering::SeqCst),
            1,
            "clear_tool_timers should run on abort"
        );
    }

    /// Test that timeout wins over abort when both are ready.
    /// This verifies the race condition fix: with biased select!,
    /// the higher-priority abort arm could incorrectly preempt the timeout.
    /// Since abort has higher priority, we test when deadline fires FIRST
    /// (at 1s) while abort fires later (at 2s). The deadline should win.
    #[tokio::test(start_paused = true)]
    async fn test_timeout_wins_over_abort_when_deadline_fires_first() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let clear_count = Arc::new(AtomicUsize::new(0));

        let clear_fn = {
            let clear_count = clear_count.clone();
            Arc::new(move || {
                clear_count.fetch_add(1, Ordering::SeqCst);
            }) as ClearToolTimersFn
        };

        // Turn future: pending (never completes)
        let turn_future = Box::pin(std::future::pending::<anyhow::Result<NatsTurnResult>>())
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<NatsTurnResult>> + Send>,
            >;

        // Abort signal: fires after 2 seconds
        let abort_signal = Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

        // Deadline: fires after 1 second (fires BEFORE abort)
        let deadline = Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

        let outcome =
            run_turn_select_loop(turn_future, abort_signal, deadline, None, clear_fn).await;

        assert!(
            matches!(outcome, TurnLoopOutcome::TimedOut),
            "deadline should win over abort since it fires first"
        );
        assert_eq!(
            clear_count.load(Ordering::SeqCst),
            1,
            "clear_tool_timers should run exactly once"
        );
    }
}
