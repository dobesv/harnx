//! Synthesized termination result for bounded non-interactive invocations.
//!
//! This module owns the **sole recognizer** for worker budget-terminal signals.
//! The constant [`BUDGET_TERMINAL_PREFIX`] and the parser [`parse_budget_terminal`]
//! are the only code that matches this prefix. All budget-terminal handling must
//! use these helpers—never ad-hoc string matching on `"harnx:budget_exceeded"`.
//!
//! Worker deadlines persist scoped timeout details in the existing Cancel requester label.
//! Budget and repetition stops use Error entries. All three share this recognizer.

mod public_progress;
mod timeout;
pub use public_progress::{PublicProgress, PUBLIC_REFERENCE_CAP};
pub use timeout::{parse_timeout_terminal, TimeoutScope, TimeoutTerminal};

use harnx_core::{
    api_types::CompletionTokenUsage,
    event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent},
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, MutexGuard};

/// Prefix for worker budget-terminal error messages.
///
/// **Invariant:** This constant and [`parse_budget_terminal`] are the sole recognizers
/// for budget terminals. No other code may match this prefix directly.
const BUDGET_TERMINAL_PREFIX: &str = "harnx:budget_exceeded ";

/// Worker-side terminal condition for a bounded invocation.
#[derive(Debug)]
pub(crate) struct TokenBudgetExceeded {
    budgeted: u64,
    budget: u64,
}

impl TokenBudgetExceeded {
    pub(crate) fn new(budgeted: u64, budget: u64) -> Self {
        Self { budgeted, budget }
    }
}

impl std::fmt::Display for TokenBudgetExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&budget_terminal_message(self.budgeted, self.budget))
    }
}

impl std::error::Error for TokenBudgetExceeded {}
/// Maximum retained bytes in the caller-side invocation thinking buffer.
pub const INVOCATION_TEXT_TAIL_CAP_BYTES: usize = 4 * 1024;

/// Delegating sink that retains a bounded tail of direct model thinking.
///
/// Public output is retained separately from thinking; neither is required for a stop result.
/// Nested [`AgentEvent::SubAgent`] chunks belong to another invocation and are not
/// included. Non-streaming model calls don't emit thought chunks, so
/// [`Self::thinking_tail`] remains empty if such a call is cancelled mid-request.
pub struct InvocationBufferingSink {
    inner: Arc<dyn AgentEventSink>,
    thinking_buf: Arc<Mutex<String>>,
    public_progress: Arc<Mutex<PublicProgress>>,
}

impl InvocationBufferingSink {
    pub fn new(inner: Arc<dyn AgentEventSink>) -> Self {
        Self {
            inner,
            thinking_buf: Arc::new(Mutex::new(String::new())),
            public_progress: Arc::new(Mutex::new(PublicProgress::default())),
        }
    }

    pub fn public_progress(&self) -> PublicProgress {
        lock_or_recover(&self.public_progress).clone()
    }

    pub fn thinking_tail(&self) -> String {
        lock_or_recover(&self.thinking_buf).clone()
    }
}

impl AgentEventSink for InvocationBufferingSink {
    fn emit(&self, event: AgentEvent) {
        self.inner.emit(event.clone());

        match &event {
            AgentEvent::Model(ModelEvent::ThoughtChunk { blocks }) => {
                append_text_blocks(&self.thinking_buf, blocks);
            }
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                for block in blocks {
                    if let ContentBlock::Text(text) = block {
                        lock_or_recover(&self.public_progress).observe_text(text);
                    }
                }
            }
            // Keep this invocation's accounting consistent with sub-agent progress:
            // nested events belong to the nested invocation.
            AgentEvent::SubAgent { .. } => {}
            _ => {}
        }
    }
}

fn append_text_blocks(buffer: &Mutex<String>, blocks: &[ContentBlock]) {
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            append_tail(buffer, text);
        }
    }
}

fn append_tail(buffer: &Mutex<String>, text: &str) {
    let mut buffer = lock_or_recover(buffer);
    buffer.push_str(text);
    if buffer.len() <= INVOCATION_TEXT_TAIL_CAP_BYTES {
        return;
    }

    let mut drain_to = buffer.len() - INVOCATION_TEXT_TAIL_CAP_BYTES;
    while !buffer.is_char_boundary(drain_to) {
        drain_to += 1;
    }
    buffer.drain(..drain_to);
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Reason an invocation was stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationKind {
    Timeout,
    BudgetExceeded,
    Repetition,
}

/// Stable token-usage shape for a synthesized termination result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TerminationUsage {
    pub input_uncached: u64,
    pub cache_write: u64,
    pub output: u64,
    pub budgeted: u64,
}

impl From<&CompletionTokenUsage> for TerminationUsage {
    fn from(usage: &CompletionTokenUsage) -> Self {
        Self {
            input_uncached: usage.uncached_input_tokens(),
            cache_write: usage.cache_write_tokens,
            output: usage.output_tokens,
            budgeted: usage.budgeted_tokens(),
        }
    }
}

/// Stable machine-readable details for a stopped invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TerminationDetails {
    pub kind: TerminationKind,
    pub session_id: String,
    pub usage: TerminationUsage,
    pub thinking_excerpt: Option<String>,
    pub retry_hint: String,
    #[serde(flatten)]
    pub timeout: Option<TimeoutTerminal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_progress: Option<PublicProgress>,
    /// What kept repeating, for a repetition stop.
    #[serde(flatten)]
    pub repetition: Option<harnx_core::loop_guard::RepetitionTerminal>,
}

/// Human-readable and machine-readable forms of one stopped invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SynthesizedResult {
    pub response: String,
    pub termination: TerminationDetails,
}

impl SynthesizedResult {
    /// Serialize the stable termination contract as a JSON object.
    pub fn termination_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.termination)
            .expect("termination details contain only JSON-compatible values")
    }
}

/// Inputs used to build a synthesized result for one stopped invocation.
pub struct TerminationInputs<'a> {
    pub kind: TerminationKind,
    pub timeout: Option<TimeoutTerminal>,
    pub public_progress: Option<&'a PublicProgress>,
    pub session_id: &'a str,
    pub usage: &'a CompletionTokenUsage,
    pub thinking_excerpt: Option<&'a str>,
    /// Required when `kind` is [`TerminationKind::BudgetExceeded`].
    pub budget: Option<u64>,
    /// Required when `kind` is [`TerminationKind::Repetition`].
    pub repetition: Option<harnx_core::loop_guard::RepetitionTerminal>,
}

/// Build the shared human-readable and structured result for a stopped invocation.
pub fn synthesize_terminated_result(inputs: TerminationInputs<'_>) -> SynthesizedResult {
    let TerminationInputs {
        kind,
        session_id,
        usage,
        thinking_excerpt,
        budget,
        repetition,
        timeout,
        public_progress,
    } = inputs;
    let usage = TerminationUsage::from(usage);
    let thinking_excerpt = thinking_excerpt
        .filter(|excerpt| !excerpt.trim().is_empty())
        .map(str::to_owned);

    let (explanation, usage_line) = match kind {
        TerminationKind::Timeout => (
            "The invocation was stopped after reaching its time limit.".to_owned(),
            format!("Usage: used {} budgeted tokens.", usage.budgeted),
        ),
        TerminationKind::BudgetExceeded => {
            let budget = budget.expect("token budget is required for a budget-exceeded result");
            (
                format!(
                    "The invocation was stopped because it reached its token budget (used {} of {} budgeted tokens).",
                    usage.budgeted, budget
                ),
                format!(
                    "Usage: used {} of {} budgeted tokens.",
                    usage.budgeted, budget
                ),
            )
        }
        TerminationKind::Repetition => {
            let terminal = repetition
                .as_ref()
                .expect("repetition details are required for a repetition result");
            (
                format!("The invocation was stopped because {}.", terminal.reason()),
                format!("Usage: used {} budgeted tokens.", usage.budgeted),
            )
        }
    };

    let thinking_section = match thinking_excerpt.as_deref() {
        Some(excerpt) => format!("--- thinking (excerpt) ---\n{excerpt}"),
        None => "No thinking text was captured (the non-streaming path produces none mid-call)."
            .to_owned(),
    };
    let retry_hint = match timeout.as_ref().map(|t| t.scope) {
        Some(scope) => scope.retry_hint(session_id),
        None if kind == TerminationKind::Repetition => format!("Inspect saved public results and change the repeated approach before continuing the same session id `{session_id}` within a live run. Do not retry unchanged."),
        None => format!("Inspect saved public results before sending revised or narrower instructions to the same session id `{session_id}` within a live run. Do not retry unchanged."),
    };
    let public_progress = public_progress.map(PublicProgress::bounded);
    let public_section = match &public_progress {
        Some(progress) if progress.available => format!(
            "Public progress (partial):\n{}\nReferences: {}",
            progress
                .output_excerpt
                .as_deref()
                .unwrap_or("No public output excerpt available."),
            progress.references.join(" ")
        ),
        _ => "Public progress unavailable; no public output or artifact references were captured."
            .to_owned(),
    };
    let response = format!(
        "{explanation}\n\n{public_section}\n\n{thinking_section}\n\n{retry_hint}\n\n{usage_line}"
    );

    SynthesizedResult {
        response,
        termination: TerminationDetails {
            kind,
            session_id: session_id.to_owned(),
            usage,
            thinking_excerpt,
            retry_hint,
            timeout,
            public_progress,
            repetition,
        },
    }
}

/// Machine-readable payload persisted when an invocation exhausts its token budget.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub struct BudgetTerminal {
    pub budgeted: u64,
    pub budget: u64,
}

/// Build the exact worker terminal message consumed by [`parse_budget_terminal`].
pub fn budget_terminal_message(budgeted: u64, budget: u64) -> String {
    format!("{BUDGET_TERMINAL_PREFIX}{{\"budgeted\":{budgeted},\"budget\":{budget}}}")
}

/// Parse a worker token-budget terminal message.
pub fn parse_budget_terminal(message: &str) -> Option<BudgetTerminal> {
    let prefix_start = message.rfind(BUDGET_TERMINAL_PREFIX)?;
    let payload = &message[prefix_start + BUDGET_TERMINAL_PREFIX.len()..];
    serde_json::from_str(payload).ok()
}

/// A worker-side stop recorded in a failed turn's error text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerTerminal {
    Timeout(TimeoutTerminal),
    BudgetExceeded(BudgetTerminal),
    Repetition(harnx_core::loop_guard::RepetitionTerminal),
}

impl WorkerTerminal {
    pub fn kind(&self) -> TerminationKind {
        match self {
            Self::Timeout(_) => TerminationKind::Timeout,
            Self::BudgetExceeded(_) => TerminationKind::BudgetExceeded,
            Self::Repetition(_) => TerminationKind::Repetition,
        }
    }

    pub fn timeout(&self) -> Option<TimeoutTerminal> {
        match self {
            Self::Timeout(t) => Some(t.clone()),
            _ => None,
        }
    }

    pub fn budget(&self) -> Option<u64> {
        match self {
            Self::BudgetExceeded(terminal) => Some(terminal.budget),
            Self::Timeout(_) | Self::Repetition(_) => None,
        }
    }

    pub fn repetition(&self) -> Option<harnx_core::loop_guard::RepetitionTerminal> {
        match self {
            Self::Timeout(_) | Self::BudgetExceeded(_) => None,
            Self::Repetition(terminal) => Some(terminal.clone()),
        }
    }
}

/// Classify a failed turn's error text as a worker-side stop, if it is one.
pub fn parse_worker_terminal(message: &str) -> Option<WorkerTerminal> {
    if let Some(timeout) = parse_timeout_terminal(message) {
        return Some(WorkerTerminal::Timeout(timeout));
    }
    if let Some(budget) = parse_budget_terminal(message) {
        return Some(WorkerTerminal::BudgetExceeded(budget));
    }
    harnx_core::loop_guard::parse_repetition_terminal(message).map(WorkerTerminal::Repetition)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_terminal_message_round_trips() {
        let terminal = BudgetTerminal {
            budgeted: 12_345,
            budget: 10_000,
        };

        assert_eq!(
            parse_budget_terminal(&budget_terminal_message(terminal.budgeted, terminal.budget)),
            Some(terminal)
        );
    }

    #[test]
    fn context_wrapped_budget_terminal_message_round_trips() {
        let terminal = BudgetTerminal {
            budgeted: 12_345,
            budget: 10_000,
        };
        let message = budget_terminal_message(terminal.budgeted, terminal.budget);
        let wrapped = anyhow::Error::msg(message.clone()).context("some worker context");
        let rendered = format!("{wrapped:#}");

        assert_eq!(rendered, format!("some worker context: {message}"));
        assert_eq!(parse_budget_terminal(&rendered), Some(terminal));
    }

    #[test]
    fn plain_error_is_not_a_budget_terminal() {
        assert_eq!(parse_budget_terminal("worker failed"), None);
    }

    #[test]
    fn malformed_budget_terminal_is_rejected() {
        assert_eq!(
            parse_budget_terminal(&format!("{BUDGET_TERMINAL_PREFIX}not-json")),
            None
        );
    }

    fn sample_usage() -> CompletionTokenUsage {
        CompletionTokenUsage {
            input_tokens: 100,
            output_tokens: 13,
            cached_tokens: 40,
            cache_write_tokens: 10,
        }
    }

    fn synthesize_sample(
        kind: TerminationKind,
        session_id: &str,
        thinking_excerpt: Option<&str>,
        budget: Option<u64>,
    ) -> SynthesizedResult {
        synthesize_terminated_result(TerminationInputs {
            timeout: None,
            public_progress: None,
            kind,
            session_id,
            usage: &sample_usage(),
            thinking_excerpt,
            budget,
            repetition: None,
        })
    }

    struct ThinkingCase<'a> {
        kind: TerminationKind,
        session_id: &'a str,
        thinking: &'a str,
        budget: Option<u64>,
        expected: &'a str,
    }

    fn assert_thinking_case(case: ThinkingCase<'_>) {
        let result =
            synthesize_sample(case.kind, case.session_id, Some(case.thinking), case.budget);
        assert_eq!(result.response, case.expected);
        assert_eq!(
            result.termination.thinking_excerpt.as_deref(),
            Some(case.thinking)
        );
        assert_eq!(result.termination.kind, case.kind);
    }

    #[test]
    fn results_with_thinking_excerpts_have_expected_text() {
        let cases = [
            ThinkingCase {
                kind: TerminationKind::Timeout,
                session_id: "session-timeout",
                thinking: "checking the final step",
                budget: None,
                expected: "The invocation was stopped after reaching its time limit.\n\n\
                           Public progress unavailable; no public output or artifact references were captured.\n\n\
                           --- thinking (excerpt) ---\nchecking the final step\n\n\
                           Inspect saved public results before sending revised or narrower instructions to the same session id `session-timeout` within a live run. Do not retry unchanged.\n\n\
                           Usage: used 73 budgeted tokens.",
            },
            ThinkingCase {
                kind: TerminationKind::BudgetExceeded,
                session_id: "session-budget",
                thinking: "the remaining work",
                budget: Some(70),
                expected: "The invocation was stopped because it reached its token budget (used 73 of 70 budgeted tokens).\n\n\
                           Public progress unavailable; no public output or artifact references were captured.\n\n\
                           --- thinking (excerpt) ---\nthe remaining work\n\n\
                           Inspect saved public results before sending revised or narrower instructions to the same session id `session-budget` within a live run. Do not retry unchanged.\n\n\
                           Usage: used 73 of 70 budgeted tokens.",
            },
        ];
        for case in cases {
            assert_thinking_case(case);
        }
    }

    #[test]
    fn timeout_result_without_thinking_excerpt_states_none_was_captured() {
        let result = synthesize_sample(
            TerminationKind::Timeout,
            "session-empty-timeout",
            Some("  \n"),
            None,
        );

        assert!(result.response.contains(
            "No thinking text was captured (the non-streaming path produces none mid-call)."
        ));
        assert!(result
            .response
            .contains("same session id `session-empty-timeout`"));
        assert!(result.response.contains("Usage: used 73 budgeted tokens."));
        assert_eq!(result.termination.thinking_excerpt, None);
    }

    #[test]
    fn budget_result_without_thinking_excerpt_states_none_was_captured() {
        let result = synthesize_sample(
            TerminationKind::BudgetExceeded,
            "session-empty-budget",
            None,
            Some(70),
        );

        assert!(result.response.starts_with(
            "The invocation was stopped because it reached its token budget (used 73 of 70 budgeted tokens)."
        ));
        assert!(result.response.contains(
            "No thinking text was captured (the non-streaming path produces none mid-call)."
        ));
        assert!(result
            .response
            .contains("same session id `session-empty-budget`"));
        assert!(result
            .response
            .ends_with("Usage: used 73 of 70 budgeted tokens."));
        assert_eq!(result.termination.thinking_excerpt, None);
    }

    #[test]
    fn termination_json_matches_stable_contract() {
        let result = synthesize_sample(
            TerminationKind::BudgetExceeded,
            "session-json",
            Some("last thought"),
            Some(70),
        );

        assert_eq!(
            result.termination_json(),
            serde_json::json!({
                "kind": "budget_exceeded",
                "session_id": "session-json",
                "usage": {
                    "input_uncached": 50,
                    "cache_write": 10,
                    "output": 13,
                    "budgeted": 73
                },
                "thinking_excerpt": "last thought",
                "retry_hint": "Inspect saved public results before sending revised or narrower instructions to the same session id `session-json` within a live run. Do not retry unchanged."
            })
        );

        let timeout =
            synthesize_sample(TerminationKind::Timeout, "session-json-timeout", None, None);
        assert_eq!(timeout.termination_json()["kind"], "timeout");
        assert_eq!(
            timeout.termination_json()["thinking_excerpt"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn repetition_result_explains_the_stop_and_carries_the_details() {
        let terminal = harnx_core::loop_guard::RepetitionTerminal::tool_calls("fs_read", 4);
        let result = synthesize_terminated_result(TerminationInputs {
            timeout: None,
            public_progress: None,
            kind: TerminationKind::Repetition,
            session_id: "child",
            usage: &sample_usage(),
            thinking_excerpt: None,
            budget: None,
            repetition: Some(terminal),
        });
        assert!(result.response.starts_with(
            "The invocation was stopped because the model kept repeating the same `fs_read` call \
             with identical arguments and results."
        ));
        let json = result.termination_json();
        assert_eq!(json["kind"], "repetition");
        assert_eq!(json["source"], "tool_calls");
        assert_eq!(json["tool"], "fs_read");
        assert_eq!(json["count"], 4);
    }

    #[test]
    fn repetition_details_sit_beside_the_stable_keys_not_in_a_nested_object() {
        let result = synthesize_terminated_result(TerminationInputs {
            timeout: None,
            public_progress: None,
            kind: TerminationKind::Repetition,
            session_id: "child",
            usage: &sample_usage(),
            thinking_excerpt: Some("same call again"),
            budget: None,
            repetition: Some(harnx_core::loop_guard::RepetitionTerminal::tool_calls(
                "fs_read", 4,
            )),
        });

        assert_eq!(
            result.termination_json(),
            serde_json::json!({
                "kind": "repetition",
                "session_id": "child",
                "usage": {
                    "input_uncached": 50,
                    "cache_write": 10,
                    "output": 13,
                    "budgeted": 73
                },
                "thinking_excerpt": "same call again",
                "retry_hint": "Inspect saved public results and change the repeated approach before continuing the same session id `child` within a live run. Do not retry unchanged.",
                "source": "tool_calls",
                "tool": "fs_read",
                "count": 4
            })
        );
    }

    #[test]
    fn timeout_and_budget_json_carry_no_repetition_keys() {
        for (kind, budget) in [
            (TerminationKind::Timeout, None),
            (TerminationKind::BudgetExceeded, Some(70)),
        ] {
            let json = synthesize_sample(kind, "session-keys", None, budget).termination_json();
            let mut keys: Vec<&str> = json
                .as_object()
                .expect("termination details serialize as an object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "kind",
                    "retry_hint",
                    "session_id",
                    "thinking_excerpt",
                    "usage"
                ],
                "{kind:?} gained keys"
            );
        }
    }

    #[test]
    fn worker_terminal_classifies_a_budget_stop() {
        let terminal = parse_worker_terminal(&budget_terminal_message(21, 20))
            .expect("budget message is a worker terminal");

        assert_eq!(
            terminal,
            WorkerTerminal::BudgetExceeded(BudgetTerminal {
                budgeted: 21,
                budget: 20,
            })
        );
        assert_eq!(
            (terminal.kind(), terminal.budget(), terminal.repetition()),
            (TerminationKind::BudgetExceeded, Some(20), None)
        );
    }

    #[test]
    fn worker_terminal_classifies_a_repetition_stop() {
        let stop = harnx_core::loop_guard::RepetitionStop(
            harnx_core::loop_guard::RepetitionTerminal::tool_calls("fs_read", 4),
        );
        let terminal =
            parse_worker_terminal(&stop.to_string()).expect("stop text is a worker terminal");

        assert_eq!(
            (terminal.kind(), terminal.budget(), terminal.repetition()),
            (TerminationKind::Repetition, None, Some(stop.0))
        );
    }

    #[test]
    fn an_output_stop_reaches_parents_with_its_source_and_no_tool() {
        let stop = harnx_core::loop_guard::RepetitionStop(
            harnx_core::loop_guard::RepetitionTerminal::output(
                harnx_core::loop_guard::RepetitionSource::Answer,
            ),
        );
        let terminal = parse_worker_terminal(&format!("model call failed: {stop}"))
            .expect("the stop text is a worker terminal");
        let result = synthesize_terminated_result(TerminationInputs {
            timeout: None,
            public_progress: None,
            kind: terminal.kind(),
            session_id: "child",
            usage: &sample_usage(),
            thinking_excerpt: None,
            budget: None,
            repetition: terminal.repetition(),
        });
        assert!(result.response.starts_with(
            "The invocation was stopped because the model's reply kept repeating the same text."
        ));
        let json = result.termination_json();
        assert_eq!(json["kind"], "repetition");
        assert_eq!(json["source"], "answer");
        assert!(json.get("tool").is_none() && json.get("count").is_none());
    }

    #[test]
    fn context_wrapped_repetition_stop_is_a_worker_terminal() {
        let stop = harnx_core::loop_guard::RepetitionTerminal::tool_calls("fs_read", 4);
        let wrapped = anyhow::Error::new(harnx_core::loop_guard::RepetitionStop(stop.clone()))
            .context("some worker context");
        let rendered = format!("{wrapped:#}");

        assert!(rendered.starts_with("some worker context: Stopped: "));
        assert_eq!(
            parse_worker_terminal(&rendered),
            Some(WorkerTerminal::Repetition(stop))
        );
    }

    #[test]
    fn plain_error_is_not_a_worker_terminal() {
        assert_eq!(parse_worker_terminal("worker failed"), None);
        assert_eq!(parse_worker_terminal("harnx:repetition not-json"), None);
        assert_eq!(parse_worker_terminal(""), None);
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl AgentEventSink for RecordingSink {
        fn emit(&self, event: AgentEvent) {
            lock_or_recover(&self.events).push(event);
        }
    }

    #[test]
    fn buffering_sink_delegates_all_events_and_captures_only_direct_thinking() {
        let inner = Arc::new(RecordingSink::default());
        let sink = InvocationBufferingSink::new(inner.clone());

        sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text("thinking".into())],
        }));
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("answer".into())],
        }));
        sink.emit(AgentEvent::SubAgent {
            source: harnx_core::event::AgentSource {
                agent: "nested".into(),
                session_id: Some("nested-session".into()),
                model: None,
            },
            event: Box::new(AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text("nested thinking".into())],
            })),
        });
        sink.emit(AgentEvent::SubAgent {
            source: harnx_core::event::AgentSource {
                agent: "nested".into(),
                session_id: Some("nested-session".into()),
                model: None,
            },
            event: Box::new(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("nested answer".into())],
            })),
        });

        assert_eq!(sink.thinking_tail(), "thinking");
        let events = lock_or_recover(&inner.events);
        assert_eq!(events.len(), 4);
        assert!(matches!(
            &events[1],
            AgentEvent::Model(ModelEvent::MessageChunk { blocks })
                if matches!(&blocks[..], [ContentBlock::Text(text)] if text == "answer")
        ));
    }

    #[test]
    fn buffering_sink_keeps_thinking_tail_at_byte_cap() {
        let sink = InvocationBufferingSink::new(Arc::new(harnx_core::event::NullSink));
        sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text(
                "a".repeat(INVOCATION_TEXT_TAIL_CAP_BYTES),
            )],
        }));
        sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text("TAIL".into())],
        }));

        let thinking = sink.thinking_tail();
        assert_eq!(thinking.len(), INVOCATION_TEXT_TAIL_CAP_BYTES);
        assert_eq!(
            thinking,
            format!(
                "{}TAIL",
                "a".repeat(INVOCATION_TEXT_TAIL_CAP_BYTES - "TAIL".len())
            )
        );
    }

    #[test]
    fn buffering_sink_caps_multibyte_thinking_on_a_char_boundary() {
        let sink = InvocationBufferingSink::new(Arc::new(harnx_core::event::NullSink));
        let burst = format!(
            "é{}",
            "x".repeat(INVOCATION_TEXT_TAIL_CAP_BYTES.saturating_sub(1))
        );
        sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text(burst)],
        }));

        let thinking = sink.thinking_tail();
        assert!(thinking.len() <= INVOCATION_TEXT_TAIL_CAP_BYTES);
        assert_eq!(
            thinking,
            "x".repeat(INVOCATION_TEXT_TAIL_CAP_BYTES.saturating_sub(1))
        );
    }
}
