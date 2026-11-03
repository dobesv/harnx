//! Inner-call execution for the `bash` script tool (issue #2537).
//!
//! [`InnerCallExecutor`] is the runtime-owned gate between the BashKit
//! interpreter (PR3) and the engine admission pipeline. It owns the resolved
//! `bash_tools` allowlist and the base provider list, and applies admission limits:
//!
//! - names outside `bash_tools` are rejected with a did-you-mean suggestion;
//! - `bash`, `switch_agent`, `ask_user`, and `*_session_handoff` are rejected
//!   even if a malformed allowlist contains them — the forked context still
//!   shares `prepare_handoff_fn`, so the executor is the guard (T2.2 review);
//! - the serialized NATS request is checked against the broker's
//!   `max_payload` before dispatch (`nats_tool_provider/request.rs` builds the
//!   payload; the probe abstraction keeps tests off NATS);
//! - each script run gets a concurrency cap (semaphore) and a call budget.
//!
//! One [`InnerCallRun`] covers one `bash` invocation: it forks the outer
//! [`ToolEvalContext`] once with the parent `tool_round` (taken from the outer
//! `ToolCallOrigin` at call time), captures the agent event sink once, and
//! wraps every inner eval in [`with_agent_event_sink`] so events emitted from
//! BashKit-spawned tasks reach the right session. Inner calls are not
//! durably recorded; their trace lives only in the `bash` ToolOutput
//! (plan decision, PR2 architecture).
//!
//! Progress aggregation lives in [`InnerProgressAggregator`]: it implements
//! [`InnerCallObserver`] and turns child lifecycle into the outer `bash`
//! tool's `ToolUpdatePatch` (running-names title + last trace lines). The
//! BashProvider passes its own `Arc<dyn ToolProgress>` to it; the engine's
//! `RuntimeToolProgress` coalesces the rest.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use harnx_core::abort::AbortSignal;
use harnx_core::event::AgentEventSink;
use harnx_core::sink::{current_agent_event_sink, with_agent_event_sink};
use harnx_core::tool::{ToolCall, ToolProgress, ToolProvider, ToolResult, ToolUpdatePatch};
use harnx_engine::tool::{
    eval_inner_tool_call, InnerToolEvalOptions, ToolApprovalRequiredError, ToolCallEmitFn,
    ToolEvalContext, ToolUpdateEmitFn, WorkBoundary,
};
use serde_json::Value;
use tokio::sync::Semaphore;

/// Cap on simultaneous inner calls per script run. `BashKit` background jobs
/// and pipelines can fan out; without a cap one script could occupy every
/// worker connection.
pub const DEFAULT_MAX_CONCURRENT_INNER_CALLS: usize = 8;

/// Cap on total inner calls per script run, on top of BashKit's own
/// `ExecutionLimits`. Bounds the worst case for a runaway loop.
pub const DEFAULT_MAX_INNER_CALLS_PER_SCRIPT: usize = 1000;

/// Maximum length of the `args_summary` trace field, in characters.
const ARGS_SUMMARY_MAX_CHARS: usize = 120;

/// An unknown name is only given a did-you-mean candidate within this
/// edit distance, so short typos suggest and garbage does not.
const MAX_SUGGESTION_EDIT_DISTANCE: usize = 2;

/// Running tool names shown in the aggregated progress title before the
/// remainder is collapsed to `+N`.
pub const AGGREGATOR_MAX_RUNNING_NAMES: usize = 3;

/// Trace lines kept in the aggregated progress markdown (the newest ones).
pub const AGGREGATOR_MAX_TRACE_LINES: usize = 10;

/// Title cap, mirroring `harnx_toolset::progress::TOOL_PROGRESS_MAX_STRING_BYTES`
/// (that module is private, so the value is repeated here).
const AGGREGATOR_MAX_TITLE_BYTES: usize = 4 * 1024;

/// Markdown cap, mirroring `TOOL_PROGRESS_MAX_MARKDOWN_BYTES`.
const AGGREGATOR_MAX_MARKDOWN_BYTES: usize = 64 * 1024;

/// Computes the serialized size of the NATS `ToolRequest` that dispatching
/// `(tool, args)` would produce. The runtime wiring (T3.4) builds the real
/// request — same fields as `nats_tool_provider/request.rs` — and serializes
/// it; keeping this a closure means tests don't need a NATS broker.
pub type RequestSizeProbe = Arc<dyn Fn(&str, &Value) -> usize + Send + Sync>;

/// Payload checks against the broker's `server_info().max_payload`, before
/// admission and again on final hook-mutated arguments just before dispatch
/// (usage precedent: `a2a_events.rs`). NATS requests aren't chunked, so an
/// oversized request would fail at publish time with a much worse error.
#[derive(Clone)]
pub struct PayloadLimit {
    pub max_bytes: usize,
    pub request_size: RequestSizeProbe,
}

impl PayloadLimit {
    fn check(&self, tool: &str, args: &Value) -> Result<(), CallRejection> {
        let request_bytes = (self.request_size)(tool, args);
        if request_bytes > self.max_bytes {
            return Err(CallRejection {
                bytes: request_bytes as u64,
                reason: format!(
                    "tool '{tool}' request is {request_bytes} bytes, above the broker's {} byte maximum payload",
                    self.max_bytes
                ),
            });
        }
        Ok(())
    }

    fn guard_dispatch(self: &Arc<Self>, ctx: &mut ToolEvalContext) {
        let limit = Arc::clone(self);
        let existing = ctx.work_boundary.take();
        ctx.work_boundary = Some(Arc::new(move |boundary, call| {
            let limit = Arc::clone(&limit);
            let existing = existing.clone();
            Box::pin(async move {
                // Only Start carries the final hook-mutated arguments that the
                // engine is about to dispatch. Accept still carries the input.
                if matches!(boundary, WorkBoundary::Start) {
                    limit.check(&call.name, &call.arguments)?;
                }
                if let Some(existing) = existing {
                    existing(boundary, call).await?;
                }
                Ok(())
            })
        }));
    }
}

/// Hook point for live inner-call activity. T2.5's progress aggregator
/// implements this to turn child lifecycle and progress into the outer
/// `bash` tool's `ToolUpdatePatch`; the executor itself only records the
/// final trace.
pub trait InnerCallObserver: Send + Sync {
    /// The forked context admitted the call; dispatch is starting.
    fn started(&self, _call: &ToolCall, _args: &Value) {}
    /// The call finished, including recoverable error-valued results.
    fn completed(&self, _call: &ToolCall, _output: &Value) {}
    /// A hook blocked or denied the call.
    fn blocked(&self, _call: &ToolCall, _reason: &str) {}
    /// A progress update for an in-flight inner call.
    fn progress(&self, _call_id: &str, _patch: &ToolUpdatePatch) {}
    /// Final trace entry for the call, recorded after any other callback.
    /// Pre-admission rejections fire this without `started`; aggregators
    /// count them as done but never as running.
    fn finished(&self, _entry: &InnerTraceEntry) {}
}

/// Configuration for the runtime-owned executor. Built in
/// `build_tool_providers` order (T3.4): base providers first, then the
/// executor, then the script provider — so `providers` here excludes the
/// script provider and recursive `bash` dispatch is structurally impossible.
pub struct InnerCallExecutorConfig {
    /// Resolved `bash_tools` inner set (`Config::bash_tool_names`).
    pub allowed: Vec<String>,
    /// Base providers, excluding the script provider.
    pub providers: Vec<Arc<dyn ToolProvider>>,
    /// Concurrent inner calls per script run; zero is clamped to one.
    pub max_concurrent: usize,
    /// Total inner calls per script run.
    pub max_calls: usize,
    /// NATS payload pre-flight; `None` skips the check (tests, non-NATS setups).
    pub payload_limit: Option<PayloadLimit>,
}

impl InnerCallExecutorConfig {
    /// Defaults: concurrency 8, budget 1000, no payload check.
    pub fn new(allowed: Vec<String>, providers: Vec<Arc<dyn ToolProvider>>) -> Self {
        Self {
            allowed,
            providers,
            max_concurrent: DEFAULT_MAX_CONCURRENT_INNER_CALLS,
            max_calls: DEFAULT_MAX_INNER_CALLS_PER_SCRIPT,
            payload_limit: None,
        }
    }

    pub fn with_caps(mut self, max_concurrent: usize, max_calls: usize) -> Self {
        self.max_concurrent = max_concurrent;
        self.max_calls = max_calls;
        self
    }

    pub fn with_payload_limit(mut self, payload_limit: PayloadLimit) -> Self {
        self.payload_limit = Some(payload_limit);
        self
    }
}

/// Runtime-owned gate for script-originated tool calls. Cheap to share via
/// `Arc`; per-script state lives in [`InnerCallRun`].
pub struct InnerCallExecutor {
    allowed: Arc<BTreeSet<String>>,
    providers: Vec<Arc<dyn ToolProvider>>,
    max_concurrent: usize,
    max_calls: usize,
    payload_limit: Option<Arc<PayloadLimit>>,
}

impl InnerCallExecutor {
    pub fn new(config: InnerCallExecutorConfig) -> Self {
        Self {
            allowed: Arc::new(config.allowed.into_iter().collect()),
            providers: config.providers,
            max_concurrent: config.max_concurrent.max(1),
            max_calls: config.max_calls,
            payload_limit: config.payload_limit.map(Arc::new),
        }
    }

    /// The resolved `bash_tools` set, for the `tools` command and help text.
    pub fn allowed_tools(&self) -> &BTreeSet<String> {
        &self.allowed
    }

    /// Start one `bash` invocation. Forks the outer context once with the
    /// parent's `tool_round` and captures the agent event sink from the
    /// calling task. The outer context is not modified.
    pub fn begin_run(
        &self,
        outer: &ToolEvalContext,
        tool_round: Option<u64>,
        observer: Option<Arc<dyn InnerCallObserver>>,
    ) -> InnerCallRun {
        // The fork replaces the outer UI emitters with collectors that feed
        // the observer; the trace is recorded from `call` itself so it also
        // covers pre-admission rejections.
        let started_observer = observer.clone();
        let emit_tool_call_fn: Arc<ToolCallEmitFn> = Arc::new(move |call, args| {
            if let Some(observer) = &started_observer {
                observer.started(call, args);
            }
        });
        let completed_observer = observer.clone();
        let emit_tool_result_fn: Arc<ToolCallEmitFn> = Arc::new(move |call, output| {
            if let Some(observer) = &completed_observer {
                observer.completed(call, output);
            }
        });
        let blocked_observer = observer.clone();
        let emit_tool_blocked_fn: Arc<ToolCallEmitFn> = Arc::new(move |call, value| {
            if let Some(observer) = &blocked_observer {
                let reason = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("blocked by hook");
                observer.blocked(call, reason);
            }
        });
        let progress_observer = observer.clone();
        let emit_tool_update_fn: Arc<ToolUpdateEmitFn> = Arc::new(move |call_id, patch| {
            if let Some(observer) = &progress_observer {
                observer.progress(call_id, patch);
            }
        });

        let mut ctx = outer.fork_for_inner_calls(InnerToolEvalOptions {
            providers: self.providers.clone(),
            tool_round,
            emit_tool_call_fn,
            emit_tool_result_fn,
            emit_tool_blocked_fn,
            emit_tool_update_fn,
        });
        if let Some(limit) = &self.payload_limit {
            limit.guard_dispatch(&mut ctx);
        }

        InnerCallRun {
            ctx,
            sink: current_agent_event_sink(),
            allowed: Arc::clone(&self.allowed),
            max_calls: self.max_calls,
            payload_limit: self.payload_limit.clone(),
            semaphore: Arc::new(Semaphore::new(self.max_concurrent)),
            observer: observer.clone(),
            next_start_seq: AtomicU64::new(0),
            issued: AtomicUsize::new(0),
            trace: Mutex::new(Vec::new()),
        }
    }
}

/// Result of one inner call from a `bash` script, already classified for the
/// script layer (T3.3 maps these to exit codes).
#[derive(Debug)]
pub enum InnerOutcome {
    /// Completed. Output is lossless: `null` stays `null` and inline media is
    /// preserved for the externalization step (PR4). Boxed: `ToolResult`
    /// carries media content parts and dwarfs the other variants.
    Ok { result: Box<ToolResult> },
    /// The provider failed recoverably; a journaled `partial_result` is kept.
    ToolError {
        message: String,
        partial_result: Option<Value>,
    },
    /// The inner confirmation deferred — the fork's confirm callback always
    /// answers Defer, so approval-needing tools are not callable from
    /// scripts. The provider was never dispatched.
    ApprovalRequired {
        tool: String,
        reason: Option<String>,
    },
    /// A PreToolUse hook blocked or denied the call.
    Blocked { reason: String },
    /// Rejected before admission: not in `bash_tools`, a control tool, an
    /// oversized NATS request, or the script's call budget ran out.
    Rejected { reason: String },
    /// The abort signal fired, the work boundary rejected the call, or the
    /// provider returned a fatal error. The whole script must stop.
    Fatal { message: String },
}

/// Per-call trace entry. Stable order: sorted by `start_seq`, the counter
/// assigned when `call` began, so completion order never reshuffles entries.
#[derive(Debug, Clone)]
pub struct InnerTraceEntry {
    pub start_seq: u64,
    pub call_id: String,
    pub name: String,
    /// Compact JSON of the arguments, at most 120 characters.
    pub args_summary: String,
    pub status: InnerTraceStatus,
    /// Serialized bytes: the result for evaluated calls, the request for
    /// payload rejections, zero otherwise.
    pub bytes: u64,
    pub duration_ms: u64,
}

impl InnerTraceEntry {
    /// One-line rendering shared by the aggregated progress markdown and the
    /// final `bash` result trace, so live and final views match:
    /// `<name> <status> <bytes>B <duration>ms <args summary>`.
    /// The args summary is dropped for empty (`null` / `{}`) arguments.
    pub fn to_progress_line(&self) -> String {
        match self.args_summary.as_str() {
            "null" | "{}" => format!(
                "{} {} {}B {}ms",
                self.name,
                self.status.as_str(),
                self.bytes,
                self.duration_ms
            ),
            summary => format!(
                "{} {} {}B {}ms {}",
                self.name,
                self.status.as_str(),
                self.bytes,
                self.duration_ms,
                summary
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InnerTraceStatus {
    Ok,
    ToolError,
    Blocked,
    ApprovalRequired,
    Rejected,
    Fatal,
}

impl InnerTraceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ToolError => "tool_error",
            Self::Blocked => "blocked",
            Self::ApprovalRequired => "approval_required",
            Self::Rejected => "rejected",
            Self::Fatal => "fatal",
        }
    }
}

/// State for one `bash` invocation. Clone-free and interior-mutable, so it
/// can be shared across the tasks BashKit spawns for `&`/`wait` pipelines.
pub struct InnerCallRun {
    ctx: ToolEvalContext,
    sink: Option<Arc<dyn AgentEventSink>>,
    allowed: Arc<BTreeSet<String>>,
    max_calls: usize,
    payload_limit: Option<Arc<PayloadLimit>>,
    semaphore: Arc<Semaphore>,
    /// Called with each final trace entry; the fork's lifecycle emitters
    /// hold their own clones for the started/completed/blocked/progress
    /// callbacks.
    observer: Option<Arc<dyn InnerCallObserver>>,
    next_start_seq: AtomicU64,
    issued: AtomicUsize,
    trace: Mutex<Vec<InnerTraceEntry>>,
}

impl InnerCallRun {
    /// The resolved `bash_tools` set, for `tools`/`help` script commands.
    pub fn allowed_tools(&self) -> &BTreeSet<String> {
        &self.allowed
    }

    /// Finalized trace entries, sorted by start order.
    pub fn trace(&self) -> Vec<InnerTraceEntry> {
        let mut entries = self.trace.lock().expect("inner trace lock").clone();
        entries.sort_by_key(|entry| entry.start_seq);
        entries
    }

    /// Execute one inner call through the full admission pipeline
    /// (PreToolUse hooks, dispatch, PostToolUse), enforcing the allowlist,
    /// call budget, and concurrency cap first. Payload limits apply before
    /// admission and again after hook mutations. The parent abort signal
    /// propagates: an aborted signal yields [`InnerOutcome::Fatal`].
    pub async fn call(&self, tool: &str, args: Value, abort_signal: &AbortSignal) -> InnerOutcome {
        let start = CallStart {
            started: Instant::now(),
            start_seq: self.next_start_seq.fetch_add(1, Ordering::SeqCst),
            call_id: uuid::Uuid::new_v4().to_string(),
            name: tool.to_string(),
            args_summary: summarize_args(&args),
        };

        if let Err(rejection) = self.preflight(tool, &args) {
            return self.reject(start, rejection.bytes, rejection.reason);
        }

        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("inner call semaphore is never closed");

        let call = ToolCall::new(start.name.clone(), args, Some(start.call_id.clone()), None);
        let evaluated = match self.sink.clone() {
            // BashKit runs commands on its own tasks, which don't inherit
            // the turn's task-local sink. Re-scope the eval so hook notices
            // and progress land in the right session.
            Some(sink) => {
                with_agent_event_sink(sink, eval_inner_tool_call(&self.ctx, call, abort_signal))
                    .await
            }
            None => eval_inner_tool_call(&self.ctx, call, abort_signal).await,
        };

        let (outcome, status, bytes) = classify_evaluation(tool, evaluated);

        self.record_trace(start, status, bytes);
        outcome
    }

    fn preflight(&self, tool: &str, args: &Value) -> Result<(), CallRejection> {
        if let Some(reason) = self.tool_rejection_reason(tool) {
            return Err(CallRejection { bytes: 0, reason });
        }
        self.check_payload_size(tool, args)?;
        if !self.reserve_budget() {
            return Err(CallRejection {
                bytes: 0,
                reason: format!(
                    "bash script exceeded its limit of {} inner calls",
                    self.max_calls
                ),
            });
        }
        Ok(())
    }

    fn tool_rejection_reason(&self, tool: &str) -> Option<String> {
        // Control tools stay at the model boundary even if a malformed
        // allowlist contains them. The fork still shares the handoff callback.
        if !crate::config::is_bash_inner_tool_allowed(tool) {
            return Some(format!(
                "tool '{tool}' cannot be called from a bash script (control tool)"
            ));
        }
        if self.allowed.contains(tool) {
            return None;
        }
        Some(match suggest_tool_name(tool, &self.allowed) {
            Some(suggestion) => {
                format!("tool '{tool}' is not in bash_tools; did you mean '{suggestion}'?")
            }
            None => format!("tool '{tool}' is not in bash_tools"),
        })
    }

    fn check_payload_size(&self, tool: &str, args: &Value) -> Result<(), CallRejection> {
        if let Some(limit) = &self.payload_limit {
            limit.check(tool, args)?;
        }
        Ok(())
    }

    /// Count the call against the budget without overshooting under races.
    fn reserve_budget(&self) -> bool {
        let mut issued = self.issued.load(Ordering::SeqCst);
        loop {
            if issued >= self.max_calls {
                return false;
            }
            match self.issued.compare_exchange_weak(
                issued,
                issued + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => issued = actual,
            }
        }
    }

    fn reject(&self, start: CallStart, bytes: u64, reason: String) -> InnerOutcome {
        self.record_trace(start, InnerTraceStatus::Rejected, bytes);
        InnerOutcome::Rejected { reason }
    }

    fn record_trace(&self, start: CallStart, status: InnerTraceStatus, bytes: u64) {
        let entry = InnerTraceEntry {
            start_seq: start.start_seq,
            call_id: start.call_id,
            name: start.name,
            args_summary: start.args_summary,
            status,
            bytes,
            duration_ms: elapsed_ms(start.started),
        };
        self.trace
            .lock()
            .expect("inner trace lock")
            .push(entry.clone());
        if let Some(observer) = &self.observer {
            observer.finished(&entry);
        }
    }
}

/// Identity and timing captured when `call` begins, shared by the rejection
/// and completion paths so every trace entry has the same shape.
struct CallStart {
    started: Instant,
    start_seq: u64,
    call_id: String,
    name: String,
    args_summary: String,
}

/// Aggregates inner-call lifecycle into the outer `bash` tool's progress.
///
/// Title: `bash: <running tool names, max 3> (<done> done, <running> running)`.
/// Markdown: the newest [`AGGREGATOR_MAX_TRACE_LINES`] trace lines. Each event
/// emits one `ToolUpdatePatch` through the outer progress handle the
/// BashProvider received in `call_tool_with_progress`; the engine's
/// `RuntimeToolProgress` already coalesces those, so this adds no timer.
///
/// Child progress patch bodies (a child's own title/markdown) are NOT
/// surfaced: the title already names running tools and nested child markdown
/// would bloat the outer row. Only lifecycle counts and trace lines show.
pub struct InnerProgressAggregator {
    progress: Arc<dyn ToolProgress>,
    state: Mutex<AggregatorState>,
}

#[derive(Default)]
struct AggregatorState {
    /// Running calls in start order: admission sequence → tool name.
    running: BTreeMap<u64, String>,
    /// call_id → admission sequence, to remove finished calls in O(1).
    call_seqs: HashMap<String, u64>,
    next_seq: u64,
    done: usize,
    /// Newest trace lines last; capped at [`AGGREGATOR_MAX_TRACE_LINES`].
    lines: Vec<String>,
}

impl InnerProgressAggregator {
    /// Wrap the outer `bash` tool's progress handle.
    pub fn new(progress: Arc<dyn ToolProgress>) -> Self {
        Self {
            progress,
            state: Mutex::new(AggregatorState::default()),
        }
    }

    fn update_running(&self, mutate: impl FnOnce(&mut AggregatorState)) {
        let patch = {
            let mut state = self.state.lock().expect("aggregator state lock");
            mutate(&mut state);
            aggregated_patch(&state)
        };
        self.progress.update(patch);
    }
}

impl InnerCallObserver for InnerProgressAggregator {
    fn started(&self, call: &ToolCall, _args: &Value) {
        // The engine assigns IDs before admission, so the Started emitter
        // always sees one.
        let call_id = call.id.clone().expect("admitted inner call has an ID");
        let name = call.name.clone();
        self.update_running(|state| {
            let seq = state.next_seq;
            state.next_seq += 1;
            state.running.insert(seq, name);
            state.call_seqs.insert(call_id, seq);
        });
    }

    fn finished(&self, entry: &InnerTraceEntry) {
        let line = entry.to_progress_line();
        self.update_running(|state| {
            // Pre-admission rejections never started; they count as done
            // without ever having been running.
            if let Some(seq) = state.call_seqs.remove(&entry.call_id) {
                state.running.remove(&seq);
            }
            state.done += 1;
            state.lines.push(line);
            let overflow = state.lines.len().saturating_sub(AGGREGATOR_MAX_TRACE_LINES);
            state.lines.drain(0..overflow);
        });
    }
}

/// Snapshot the aggregate state as one patch. Bounded defensively: the
/// trace summary stays far below the toolset limits, but a pathological tool
/// name or args summary must not push the outer row over them.
fn aggregated_patch(state: &AggregatorState) -> ToolUpdatePatch {
    ToolUpdatePatch {
        title: Some(truncate_bytes(
            aggregated_title(state),
            AGGREGATOR_MAX_TITLE_BYTES,
        )),
        // Omit markdown while no call has finished yet, so the patch doesn't
        // clear the outer row's initial content.
        markdown: (!state.lines.is_empty())
            .then(|| truncate_bytes(state.lines.join("\n"), AGGREGATOR_MAX_MARKDOWN_BYTES)),
        ..ToolUpdatePatch::default()
    }
}

fn aggregated_title(state: &AggregatorState) -> String {
    let mut title = String::from("bash:");
    let names: Vec<&str> = state.running.values().map(String::as_str).collect();
    if !names.is_empty() {
        title.push(' ');
        title.push_str(
            &names
                .iter()
                .take(AGGREGATOR_MAX_RUNNING_NAMES)
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
        );
        if names.len() > AGGREGATOR_MAX_RUNNING_NAMES {
            title.push_str(&format!(" +{}", names.len() - AGGREGATOR_MAX_RUNNING_NAMES));
        }
    }
    title.push_str(&format!(
        " ({} done, {} running)",
        state.done,
        state.running.len()
    ));
    title
}

/// Truncate at a UTF-8 character boundary so the byte cap can't split a
/// multi-byte character.
fn truncate_bytes(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn serialized_size(value: &Value) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or(0)
}

fn summarize_args(args: &Value) -> String {
    let text = args.to_string();
    if text.chars().count() <= ARGS_SUMMARY_MAX_CHARS {
        return text;
    }
    let mut truncated: String = text.chars().take(ARGS_SUMMARY_MAX_CHARS - 1).collect();
    truncated.push('…');
    truncated
}

fn suggest_tool_name<'a>(name: &str, allowed: &'a BTreeSet<String>) -> Option<&'a str> {
    let name_chars: Vec<char> = name.chars().collect();
    allowed
        .iter()
        .map(|candidate| (candidate, edit_distance(&name_chars, candidate)))
        .filter(|&(_, distance)| distance <= MAX_SUGGESTION_EDIT_DISTANCE)
        .min_by_key(|&(_, distance)| distance)
        .map(|(candidate, _)| candidate.as_str())
}

/// Plain Levenshtein distance over characters. The allowed set is small, so
/// the O(n·m) scan per candidate beats pulling in a fuzzy-matching crate.
fn edit_distance(a: &[char], b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    for (index, a_char) in a.iter().enumerate() {
        let mut current = vec![index + 1];
        for (b_index, b_char) in b_chars.iter().enumerate() {
            let substitution_cost = usize::from(a_char != b_char);
            current.push(
                (previous[b_index] + substitution_cost)
                    .min(previous[b_index + 1] + 1)
                    .min(current[b_index] + 1),
            );
        }
        previous = current;
    }
    previous[b_chars.len()]
}

#[derive(Debug)]
struct CallRejection {
    bytes: u64,
    reason: String,
}

impl std::fmt::Display for CallRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for CallRejection {}

fn classify_evaluation(
    tool: &str,
    evaluated: anyhow::Result<ToolResult>,
) -> (InnerOutcome, InnerTraceStatus, u64) {
    match evaluated {
        Ok(result) => {
            let bytes = serialized_size(&result.output);
            let (outcome, status) = classify_output(result);
            (outcome, status, bytes)
        }
        Err(error) => {
            if let Some(rejection) = error.downcast_ref::<CallRejection>() {
                return (
                    InnerOutcome::Rejected {
                        reason: rejection.reason.clone(),
                    },
                    InnerTraceStatus::Rejected,
                    rejection.bytes,
                );
            }
            let (outcome, status) = classify_error(tool, error);
            (outcome, status, 0)
        }
    }
}

fn classify_output(result: ToolResult) -> (InnerOutcome, InnerTraceStatus) {
    if result
        .output
        .get("blocked_by_hook")
        .and_then(Value::as_bool)
        == Some(true)
    {
        let reason = result
            .output
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("blocked by hook")
            .to_string();
        return (InnerOutcome::Blocked { reason }, InnerTraceStatus::Blocked);
    }
    if result.output.get("is_error").and_then(Value::as_bool) == Some(true) {
        let message = result
            .output
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("tool error")
            .to_string();
        let partial_result = result.output.get("partial_result").cloned();
        return (
            InnerOutcome::ToolError {
                message,
                partial_result,
            },
            InnerTraceStatus::ToolError,
        );
    }
    (
        InnerOutcome::Ok {
            result: Box::new(result),
        },
        InnerTraceStatus::Ok,
    )
}

fn classify_error(tool: &str, error: anyhow::Error) -> (InnerOutcome, InnerTraceStatus) {
    if let Some(approval) = error.downcast_ref::<ToolApprovalRequiredError>() {
        let reason = approval
            .deferred_calls()
            .first()
            .and_then(|deferred| deferred.reason.clone());
        return (
            InnerOutcome::ApprovalRequired {
                tool: tool.to_string(),
                reason,
            },
            InnerTraceStatus::ApprovalRequired,
        );
    }
    (
        InnerOutcome::Fatal {
            message: format!("{error:#}"),
        },
        InnerTraceStatus::Fatal,
    )
}

#[cfg(test)]
mod tests {
    mod payload;

    use super::*;
    use harnx_core::abort::create_abort_signal;
    use harnx_core::event::{AgentEvent, NoticeEvent};
    use harnx_core::hooks::{HookEvent, HookOutcome, HookResult, HookResultControl};
    use harnx_core::instance::ServerScope;
    use harnx_core::partial_result::error_with_partial_result;
    use harnx_core::tool::{ToolCallOrigin, ToolError, ToolProgress, ToolProviderOutput};
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    type HookEvents = Arc<Mutex<Vec<HookEvent>>>;

    fn continue_outcome() -> HookOutcome {
        HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult::default(),
        }
    }

    enum HookControl {
        Continue,
        Block(&'static str),
        Ask(Option<&'static str>),
        Mutate(Value),
    }

    /// Outer context with panicking emitters and confirmation: the fork must
    /// replace all of them, and any inner-call leak through the outer
    /// callbacks fails the test instead of silently creating child UI rows.
    fn outer_context(
        providers: Vec<Arc<dyn ToolProvider>>,
        control: HookControl,
    ) -> (ToolEvalContext, HookEvents) {
        let events: HookEvents = Arc::default();
        let captured = Arc::clone(&events);
        let control = Arc::new(control);
        let ctx = ToolEvalContext {
            work_boundary: None,
            instance_id: ServerScope::new(),
            tool_round: Some(1),
            render: None,
            providers,
            allowed_tool_names: HashSet::new(),
            current_agent_package: None,
            handoff_targets: HashMap::new(),
            emit_tool_call_fn: Arc::new(|_, _| {
                panic!("outer Started emitter fired for an inner call")
            }),
            emit_tool_result_fn: Arc::new(|_, _| {
                panic!("outer Completed emitter fired for an inner call")
            }),
            emit_tool_blocked_fn: Arc::new(|_, _| {
                panic!("outer Blocked emitter fired for an inner call")
            }),
            emit_tool_update_fn: Arc::new(|_, _| {
                panic!("outer progress emitter fired for an inner call")
            }),
            confirm_tool_use_fn: Arc::new(|_, _, _| {
                panic!("outer confirmation fired for an inner call")
            }),
            dispatch_hook_fn: Arc::new(move |event| {
                let outcome = match &*control {
                    HookControl::Continue => continue_outcome(),
                    HookControl::Mutate(arguments) => HookOutcome {
                        control: HookResultControl::Continue,
                        result: HookResult {
                            mutated_tool_input: Some(arguments.clone()),
                            ..HookResult::default()
                        },
                    },
                    HookControl::Block(reason) => HookOutcome {
                        control: HookResultControl::Block {
                            reason: reason.to_string(),
                        },
                        result: HookResult::default(),
                    },
                    HookControl::Ask(reason) => HookOutcome {
                        control: HookResultControl::Ask {
                            reason: reason.map(str::to_string),
                        },
                        result: HookResult::default(),
                    },
                };
                captured.lock().expect("hook events lock").push(event);
                Box::pin(async move { outcome })
            }),
            prepare_handoff_fn: None,
        };
        (ctx, events)
    }

    #[derive(Debug)]
    struct RecordedDispatch {
        tool: String,
        args: Value,
        call_id: Option<String>,
        tool_round: Option<u64>,
    }

    #[derive(Default)]
    struct DispatchLog {
        calls: Mutex<Vec<RecordedDispatch>>,
        concurrent: AtomicUsize,
        max_concurrent: AtomicUsize,
    }

    struct InFlightDispatch<'a>(&'a DispatchLog);

    impl Drop for InFlightDispatch<'_> {
        fn drop(&mut self) {
            self.0.concurrent.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl DispatchLog {
        fn record(
            &self,
            tool: &str,
            args: &Value,
            origin: ToolCallOrigin<'_>,
        ) -> InFlightDispatch<'_> {
            let now_concurrent = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrent
                .fetch_max(now_concurrent, Ordering::SeqCst);
            self.calls
                .lock()
                .expect("dispatch log lock")
                .push(RecordedDispatch {
                    tool: tool.to_string(),
                    args: args.clone(),
                    call_id: origin.tool_call_id.map(str::to_string),
                    tool_round: origin.tool_round,
                });
            InFlightDispatch(self)
        }

        fn count(&self) -> usize {
            self.calls.lock().expect("dispatch log lock").len()
        }
    }

    enum Behavior {
        Null,
        Ok(Value),
        DelayedOk(Duration, Value),
        Recoverable {
            message: &'static str,
            partial: Option<Value>,
        },
        NoticeThenOk(String, Value),
        Panic,
    }

    struct FakeToolProvider {
        log: Arc<DispatchLog>,
        /// `None` handles any name; the allowlist is the only gate being tested.
        handles: Option<HashSet<&'static str>>,
        progress_title: Option<String>,
        behavior: Behavior,
    }

    impl FakeToolProvider {
        fn has(&self, tool_name: &str) -> bool {
            self.handles
                .as_ref()
                .is_none_or(|names| names.contains(tool_name))
        }

        async fn run(&self, _arguments: &Value) -> Result<ToolProviderOutput, ToolError> {
            match &self.behavior {
                Behavior::Null => Ok(ToolProviderOutput::new(Value::Null)),
                Behavior::Ok(value) => Ok(ToolProviderOutput::new(value.clone())),
                Behavior::DelayedOk(delay, value) => {
                    tokio::time::sleep(*delay).await;
                    Ok(ToolProviderOutput::new(value.clone()))
                }
                Behavior::Recoverable { message, partial } => {
                    Err(ToolError::Recoverable(error_with_partial_result(
                        anyhow::anyhow!(*message),
                        partial.clone().unwrap_or(Value::Null),
                    )))
                }
                Behavior::NoticeThenOk(notice, value) => {
                    harnx_core::sink::emit_agent_event(AgentEvent::Notice(NoticeEvent::Info(
                        notice.clone(),
                    )));
                    Ok(ToolProviderOutput::new(value.clone()))
                }
                Behavior::Panic => panic!("provider must not be dispatched"),
            }
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for FakeToolProvider {
        fn name(&self) -> &str {
            "fake-tool-provider"
        }

        fn has_tool(&self, tool_name: &str) -> bool {
            self.has(tool_name)
        }

        async fn call_tool_with_progress(
            &self,
            tool_name: &str,
            arguments: Value,
            origin: ToolCallOrigin<'_>,
            _abort: &AbortSignal,
            progress: Arc<dyn ToolProgress>,
        ) -> Result<ToolProviderOutput, ToolError> {
            let _in_flight = self.log.record(tool_name, &arguments, origin);
            if let Some(title) = &self.progress_title {
                progress.update(ToolUpdatePatch {
                    title: Some(title.clone()),
                    ..ToolUpdatePatch::default()
                });
            }
            self.run(&arguments).await
        }

        async fn call_tool(
            &self,
            tool_name: &str,
            arguments: Value,
            abort: &AbortSignal,
        ) -> Result<ToolProviderOutput, ToolError> {
            self.call_tool_with_progress(
                tool_name,
                arguments,
                ToolCallOrigin::default(),
                abort,
                Arc::new(NoopProgress),
            )
            .await
        }
    }

    struct NoopProgress;

    impl ToolProgress for NoopProgress {
        fn update(&self, _patch: ToolUpdatePatch) {}
    }

    fn executor_with(
        allowed: &[&str],
        providers: Vec<Arc<dyn ToolProvider>>,
        caps: (usize, usize),
    ) -> InnerCallExecutor {
        InnerCallExecutor::new(
            InnerCallExecutorConfig::new(
                allowed.iter().map(|name| name.to_string()).collect(),
                providers,
            )
            .with_caps(caps.0, caps.1),
        )
    }

    /// Same provider list for the outer context and the executor: the fork
    /// dispatches through the executor's base providers, exactly as the
    /// runtime wiring will (build base providers, then the executor).
    fn shared(provider: &Arc<FakeToolProvider>) -> Vec<Arc<dyn ToolProvider>> {
        vec![as_dyn(provider)]
    }

    fn as_dyn(provider: &Arc<FakeToolProvider>) -> Arc<dyn ToolProvider> {
        provider.clone()
    }

    const PARENT_ROUND: Option<u64> = Some(42);

    #[tokio::test]
    async fn concurrent_calls_respect_cap() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::DelayedOk(Duration::from_millis(40), json!("done")),
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (2, 100));
        let run = Arc::new(executor.begin_run(&ctx, PARENT_ROUND, None));
        let abort = create_abort_signal();

        let mut handles = Vec::new();
        for _ in 0..6 {
            let run = Arc::clone(&run);
            let abort = Arc::clone(&abort);
            handles.push(tokio::spawn(async move {
                run.call("tool_a", json!({"n": 1}), &abort).await
            }));
        }
        for handle in handles {
            assert!(matches!(
                handle.await.expect("call task"),
                InnerOutcome::Ok { .. }
            ));
        }

        assert_eq!(log.count(), 6);
        assert!(
            log.max_concurrent.load(Ordering::SeqCst) <= 2,
            "concurrency cap was exceeded: {}",
            log.max_concurrent.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn trace_order_is_stable_by_start_even_when_completion_order_differs() {
        let slow = Arc::new(FakeToolProvider {
            log: Arc::new(DispatchLog::default()),
            handles: Some(["slow_tool"].into_iter().collect()),
            progress_title: None,
            behavior: Behavior::DelayedOk(Duration::from_millis(80), json!("slow")),
        });
        let fast = Arc::new(FakeToolProvider {
            log: Arc::new(DispatchLog::default()),
            handles: Some(["fast_tool"].into_iter().collect()),
            progress_title: None,
            behavior: Behavior::DelayedOk(Duration::from_millis(5), json!("fast")),
        });
        let (ctx, _hooks) =
            outer_context(vec![as_dyn(&slow), as_dyn(&fast)], HookControl::Continue);
        let executor = executor_with(
            &["slow_tool", "fast_tool"],
            vec![as_dyn(&slow), as_dyn(&fast)],
            (8, 100),
        );
        let run = Arc::new(executor.begin_run(&ctx, PARENT_ROUND, None));
        let abort = create_abort_signal();

        let slow_task = {
            let run = Arc::clone(&run);
            let abort = Arc::clone(&abort);
            tokio::spawn(async move { run.call("slow_tool", json!({}), &abort).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        let fast_task = {
            let run = Arc::clone(&run);
            let abort = Arc::clone(&abort);
            tokio::spawn(async move { run.call("fast_tool", json!({}), &abort).await })
        };
        assert!(matches!(
            fast_task.await.expect("fast call"),
            InnerOutcome::Ok { .. }
        ));
        assert!(matches!(
            slow_task.await.expect("slow call"),
            InnerOutcome::Ok { .. }
        ));

        let trace = run.trace();
        assert_eq!(trace.len(), 2);
        for (entry, (name, seq)) in trace.iter().zip([("slow_tool", 0), ("fast_tool", 1)]) {
            assert_eq!(
                (entry.name.as_str(), entry.start_seq, entry.status),
                (name, seq, InnerTraceStatus::Ok)
            );
        }
    }

    #[tokio::test]
    async fn unknown_tool_is_rejected_before_admission_with_suggestion() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Panic,
        });
        let (ctx, hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(
            &["fs_read", "fs_write", "grep_search"],
            shared(&provider),
            (8, 100),
        );
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        let typo = run.call("fs_readd", json!({}), &abort).await;
        match typo {
            InnerOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("did you mean 'fs_read'"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected rejection, got {other:?}"),
        }

        let garbage = run.call("zzzz", json!({}), &abort).await;
        match garbage {
            InnerOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("not in bash_tools"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected rejection, got {other:?}"),
        }

        assert_eq!(log.count(), 0, "provider must not be dispatched");
        assert!(
            hooks.lock().expect("hook events lock").is_empty(),
            "rejection happens before admission, so no hooks may fire"
        );
        let trace = run.trace();
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[0].status, InnerTraceStatus::Rejected);
        assert_eq!(trace[1].status, InnerTraceStatus::Rejected);
    }

    #[tokio::test]
    async fn control_tools_rejected_even_when_allowlisted() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Panic,
        });
        let (ctx, hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(
            &["bash", "switch_agent", "ask_user", "atlas_session_handoff"],
            shared(&provider),
            (8, 100),
        );
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        for name in ["bash", "switch_agent", "ask_user", "atlas_session_handoff"] {
            match run.call(name, json!({}), &abort).await {
                InnerOutcome::Rejected { reason } => {
                    assert!(
                        reason.contains("cannot be called from a bash script"),
                        "unexpected reason for {name}: {reason}"
                    );
                }
                other => panic!("expected rejection for {name}, got {other:?}"),
            }
        }

        assert_eq!(log.count(), 0, "control tools must never dispatch");
        assert!(hooks.lock().expect("hook events lock").is_empty());
    }

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<String>>,
    }

    impl harnx_core::event::AgentEventSink for CollectingSink {
        fn emit(&self, event: AgentEvent) {
            let AgentEvent::Notice(NoticeEvent::Info(message)) = event else {
                panic!("unexpected event: {event:?}")
            };
            self.events.lock().expect("sink lock").push(message);
        }
    }

    #[tokio::test]
    async fn spawned_inner_calls_route_events_to_the_captured_sink() {
        let sink_a = Arc::new(CollectingSink::default());
        let sink_b = Arc::new(CollectingSink::default());
        let provider_a = Arc::new(FakeToolProvider {
            log: Arc::new(DispatchLog::default()),
            handles: Some(["tool_a"].into_iter().collect()),
            progress_title: None,
            behavior: Behavior::NoticeThenOk("notice-a".into(), json!("a")),
        });
        let provider_b = Arc::new(FakeToolProvider {
            log: Arc::new(DispatchLog::default()),
            handles: Some(["tool_b"].into_iter().collect()),
            progress_title: None,
            behavior: Behavior::NoticeThenOk("notice-b".into(), json!("b")),
        });
        let (ctx_a, _hooks_a) = outer_context(shared(&provider_a), HookControl::Continue);
        let (ctx_b, _hooks_b) = outer_context(shared(&provider_b), HookControl::Continue);
        let executor = executor_with(
            &["tool_a", "tool_b"],
            vec![as_dyn(&provider_a), as_dyn(&provider_b)],
            (8, 100),
        );

        // Each run captures the sink of the task that starts it, as the
        // worker does per turn.
        let run_a = with_agent_event_sink(Arc::clone(&sink_a) as Arc<dyn AgentEventSink>, async {
            executor.begin_run(&ctx_a, PARENT_ROUND, None)
        })
        .await;
        let run_b = with_agent_event_sink(Arc::clone(&sink_b) as Arc<dyn AgentEventSink>, async {
            executor.begin_run(&ctx_b, PARENT_ROUND, None)
        })
        .await;

        // BashKit runs commands on spawned tasks with no task-local sink.
        let run_a = Arc::new(run_a);
        let run_b = Arc::new(run_b);
        let task_a = {
            let run = Arc::clone(&run_a);
            let abort = create_abort_signal();
            tokio::spawn(async move { run.call("tool_a", json!({}), &abort).await })
        };
        let task_b = {
            let run = Arc::clone(&run_b);
            let abort = create_abort_signal();
            tokio::spawn(async move { run.call("tool_b", json!({}), &abort).await })
        };
        for (name, task) in [("call a", task_a), ("call b", task_b)] {
            assert!(matches!(task.await.expect(name), InnerOutcome::Ok { .. }));
        }

        assert_eq!(
            sink_a.events.lock().expect("sink lock").clone(),
            ["notice-a"]
        );
        assert_eq!(
            sink_b.events.lock().expect("sink lock").clone(),
            ["notice-b"]
        );
    }

    #[tokio::test]
    async fn deferred_approval_maps_to_approval_required_without_dispatch() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Panic,
        });
        let (ctx, hooks) = outer_context(shared(&provider), HookControl::Ask(Some("needs review")));
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        match run.call("tool_a", json!({"x": 1}), &abort).await {
            InnerOutcome::ApprovalRequired { tool, reason } => {
                assert_eq!(tool, "tool_a");
                assert_eq!(reason.as_deref(), Some("needs review"));
            }
            other => panic!("expected approval_required, got {other:?}"),
        }

        assert_eq!(log.count(), 0, "deferred calls must not dispatch");
        let hooks = hooks.lock().expect("hook events lock");
        assert_eq!(hooks.len(), 1, "only the PreToolUse hook fires");
        assert!(matches!(hooks[0], HookEvent::PreToolUse { .. }));
        drop(hooks);
        assert_eq!(run.trace()[0].status, InnerTraceStatus::ApprovalRequired);
    }

    #[tokio::test]
    async fn blocked_hook_maps_to_blocked() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Panic,
        });
        let (ctx, _hooks) =
            outer_context(shared(&provider), HookControl::Block("denied by policy"));
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        match run.call("tool_a", json!({}), &abort).await {
            InnerOutcome::Blocked { reason } => assert_eq!(reason, "denied by policy"),
            other => panic!("expected blocked, got {other:?}"),
        }
        assert_eq!(log.count(), 0);
        assert_eq!(run.trace()[0].status, InnerTraceStatus::Blocked);
    }

    #[tokio::test]
    async fn provider_error_maps_to_tool_error_with_partial_result() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Recoverable {
                message: "boom",
                partial: Some(json!({"steps": 3})),
            },
        });
        let (ctx, hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        match run.call("tool_a", json!({}), &abort).await {
            InnerOutcome::ToolError {
                message,
                partial_result,
            } => {
                assert!(message.contains("boom"), "unexpected message: {message}");
                assert_eq!(partial_result, Some(json!({"steps": 3})));
            }
            other => panic!("expected tool error, got {other:?}"),
        }

        assert_eq!(log.count(), 1);
        let hooks = hooks.lock().expect("hook events lock");
        assert!(hooks
            .iter()
            .any(|event| matches!(event, HookEvent::PostToolUseFailure { .. })));
        drop(hooks);
        assert_eq!(run.trace()[0].status, InnerTraceStatus::ToolError);
    }

    #[tokio::test]
    async fn abort_maps_to_fatal() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Ok(json!("late")),
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();
        abort.set_ctrlc();

        match run.call("tool_a", json!({}), &abort).await {
            InnerOutcome::Fatal { message } => {
                assert!(
                    message.contains("interrupted"),
                    "unexpected fatal message: {message}"
                );
            }
            other => panic!("expected fatal, got {other:?}"),
        }
        assert_eq!(log.count(), 0);
        assert_eq!(run.trace()[0].status, InnerTraceStatus::Fatal);
    }

    #[tokio::test]
    async fn budget_cap_rejects_extra_calls() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Ok(json!("done")),
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 3));
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        for _ in 0..3 {
            assert!(matches!(
                run.call("tool_a", json!({}), &abort).await,
                InnerOutcome::Ok { .. }
            ));
        }
        match run.call("tool_a", json!({}), &abort).await {
            InnerOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("exceeded its limit of 3 inner calls"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected budget rejection, got {other:?}"),
        }
        assert_eq!(log.count(), 3);
        let trace = run.trace();
        assert_eq!(trace.len(), 4);
        assert_eq!(trace[3].status, InnerTraceStatus::Rejected);
    }

    #[tokio::test]
    async fn payload_pre_flight_rejects_oversize_requests() {
        let log = Arc::new(DispatchLog::default());
        // Ok behavior: the small request must dispatch; the oversized one
        // is stopped before admission and only the trace records it.
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Ok(json!("done")),
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        // Probe mirrors the runtime wiring: full serialized request size.
        let request_size: RequestSizeProbe = Arc::new(|_tool, args| {
            serde_json::to_vec(args)
                .map(|bytes| bytes.len() + 64)
                .unwrap_or(usize::MAX)
        });
        let executor = InnerCallExecutor::new(
            InnerCallExecutorConfig::new(vec!["tool_a".into()], shared(&provider))
                .with_caps(8, 100)
                .with_payload_limit(PayloadLimit {
                    max_bytes: 120,
                    request_size,
                }),
        );
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        let big_args = json!({"blob": "x".repeat(200)});
        let oversized = run.call("tool_a", big_args.clone(), &abort).await;
        let request_bytes = serde_json::to_vec(&big_args).expect("args serialize").len() + 64;
        match oversized {
            InnerOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("tool_a")
                        && reason.contains(&request_bytes.to_string())
                        && reason.contains("120"),
                    "reason must name tool, size, and limit: {reason}"
                );
            }
            other => panic!("expected payload rejection, got {other:?}"),
        }

        // A small request on the same run still works.
        let small = json!({"n": 1});
        assert!(matches!(
            run.call("tool_a", small, &abort).await,
            InnerOutcome::Ok { .. }
        ));
        assert_eq!(log.count(), 1, "only the small request dispatches");
        let trace = run.trace();
        assert_eq!(trace[0].bytes, request_bytes as u64);
        assert_eq!(trace[0].status, InnerTraceStatus::Rejected);
    }

    #[tokio::test]
    async fn null_result_is_preserved_in_ok() {
        let provider = Arc::new(FakeToolProvider {
            log: Arc::new(DispatchLog::default()),
            handles: None,
            progress_title: None,
            behavior: Behavior::Null,
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        match run.call("tool_a", json!({}), &abort).await {
            InnerOutcome::Ok { result } => assert_eq!(result.output, Value::Null),
            other => panic!("expected ok, got {other:?}"),
        }
        // "null" serialized is 4 bytes.
        assert_eq!(run.trace()[0].bytes, 4);
    }

    fn assert_dispatch_origin(call: &RecordedDispatch, id: &str) {
        assert_eq!((call.tool.as_str(), &call.args), ("tool_a", &json!({})));
        assert_eq!(call.call_id.as_deref(), Some(id));
        assert_eq!(call.tool_round, PARENT_ROUND);
    }

    #[tokio::test]
    async fn child_call_ids_are_unique_and_parent_round_propagates() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::Ok(json!("done")),
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        let first = run.call("tool_a", json!({}), &abort).await;
        let second = run.call("tool_a", json!({}), &abort).await;
        let (InnerOutcome::Ok { result: first }, InnerOutcome::Ok { result: second }) =
            (first, second)
        else {
            panic!("expected both calls to succeed")
        };
        let first_id = first.call.id.clone().expect("engine preserves supplied id");
        let second_id = second
            .call
            .id
            .clone()
            .expect("engine preserves supplied id");
        assert_ne!(first_id, second_id);

        let calls = log.calls.lock().expect("dispatch log lock");
        assert_eq!(calls.len(), 2);
        for (call, id) in calls.iter().zip([&first_id, &second_id]) {
            assert_dispatch_origin(call, id);
        }
        drop(calls);

        let trace = run.trace();
        assert_eq!(trace[0].call_id, first_id);
        assert_eq!(trace[1].call_id, second_id);
    }

    #[tokio::test]
    async fn long_args_summary_is_truncated_to_120_chars() {
        let provider = Arc::new(FakeToolProvider {
            log: Arc::new(DispatchLog::default()),
            handles: None,
            progress_title: None,
            behavior: Behavior::Ok(json!("done")),
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);
        let abort = create_abort_signal();

        let long = json!({"text": "y".repeat(400)});
        assert!(matches!(
            run.call("tool_a", long, &abort).await,
            InnerOutcome::Ok { .. }
        ));

        let summary = &run.trace()[0].args_summary;
        assert_eq!(summary.chars().count(), ARGS_SUMMARY_MAX_CHARS);
        assert!(summary.ends_with('…'));
    }

    #[derive(Default)]
    struct RecordingObserver {
        started: Mutex<Vec<String>>,
        completed: Mutex<Vec<String>>,
        blocked: Mutex<Vec<String>>,
        progress: Mutex<Vec<(String, String)>>,
    }

    impl InnerCallObserver for RecordingObserver {
        fn started(&self, call: &ToolCall, _args: &Value) {
            self.started
                .lock()
                .expect("observer lock")
                .push(call.name.clone());
        }

        fn completed(&self, call: &ToolCall, _output: &Value) {
            self.completed
                .lock()
                .expect("observer lock")
                .push(call.name.clone());
        }

        fn blocked(&self, call: &ToolCall, reason: &str) {
            self.blocked
                .lock()
                .expect("observer lock")
                .push(format!("{}: {reason}", call.name));
        }

        fn progress(&self, call_id: &str, patch: &ToolUpdatePatch) {
            self.progress
                .lock()
                .expect("observer lock")
                .push((call_id.to_string(), patch.title.clone().unwrap_or_default()));
        }
    }

    #[tokio::test]
    async fn observer_receives_lifecycle_and_progress_events() {
        let provider = Arc::new(FakeToolProvider {
            log: Arc::new(DispatchLog::default()),
            handles: None,
            progress_title: Some("working".into()),
            behavior: Behavior::Ok(json!("done")),
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let observer = Arc::new(RecordingObserver::default());
        let run = executor.begin_run(&ctx, PARENT_ROUND, Some(observer.clone()));
        let abort = create_abort_signal();

        assert!(matches!(
            run.call("tool_a", json!({}), &abort).await,
            InnerOutcome::Ok { .. }
        ));
        assert_eq!(
            observer.started.lock().expect("lock").as_slice(),
            ["tool_a"]
        );
        assert_eq!(
            observer.completed.lock().expect("lock").as_slice(),
            ["tool_a"]
        );
        let call_id = run.trace()[0].call_id.clone();
        assert_eq!(
            observer.progress.lock().expect("lock").as_slice(),
            [(call_id, "working".to_string())]
        );
        assert!(observer.blocked.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn blocked_call_reaches_observer_and_trace() {
        let provider = Arc::new(FakeToolProvider {
            log: Arc::new(DispatchLog::default()),
            handles: None,
            progress_title: None,
            behavior: Behavior::Panic,
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Block("nope"));
        let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
        let observer = Arc::new(RecordingObserver::default());
        let run = executor.begin_run(&ctx, PARENT_ROUND, Some(observer.clone()));
        let abort = create_abort_signal();

        assert!(matches!(
            run.call("tool_a", json!({}), &abort).await,
            InnerOutcome::Blocked { .. }
        ));
        assert_eq!(
            observer.blocked.lock().expect("lock").as_slice(),
            ["tool_a: nope"]
        );
    }

    #[test]
    fn suggestion_distance_follows_edit_distance_rules() {
        let allowed: BTreeSet<String> = ["fs_read", "fs_write", "grep_search"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for (name, expected) in [
            ("fs_readd", Some("fs_read")),
            ("grep_sarch", Some("grep_search")),
            ("fs_redddd", None), // Distance 3: no suggestion.
            ("zzz", None),
        ] {
            assert_eq!(suggest_tool_name(name, &allowed), expected, "{name}");
        }
    }

    #[test]
    fn args_summary_handles_null_and_short_values() {
        assert_eq!(summarize_args(&Value::Null), "null");
        assert_eq!(summarize_args(&json!({"a": 1})), r#"{"a":1}"#);
    }

    // ================================================================
    // T2.5 progress aggregation
    // ================================================================

    #[derive(Default)]
    struct RecordingProgress {
        patches: Mutex<Vec<ToolUpdatePatch>>,
    }

    impl ToolProgress for RecordingProgress {
        fn update(&self, patch: ToolUpdatePatch) {
            self.patches.lock().expect("patch log lock").push(patch);
        }
    }

    fn fake_entry(
        seq: u64,
        name: &str,
        status: InnerTraceStatus,
        summary: &str,
    ) -> InnerTraceEntry {
        InnerTraceEntry {
            start_seq: seq,
            call_id: format!("call-{seq}"),
            name: name.to_string(),
            args_summary: summary.to_string(),
            status,
            bytes: 42,
            duration_ms: 7,
        }
    }

    fn started_call(name: &str, id: &str) -> ToolCall {
        ToolCall::new(name.to_string(), json!({}), Some(id.to_string()), None)
    }

    /// Parse `(<done> done, <running> running)` out of an aggregated title.
    fn title_counts(title: &str) -> (usize, usize) {
        let counts = title
            .rsplit_once('(')
            .unwrap_or_else(|| panic!("title missing counts: {title}"))
            .1
            .trim_end_matches(')');
        let (done, running) = counts
            .split_once(" done, ")
            .unwrap_or_else(|| panic!("malformed counts: {title}"));
        (
            done.trim().parse().expect("done count"),
            running
                .trim_end_matches(" running")
                .parse()
                .expect("running count"),
        )
    }

    #[test]
    fn aggregator_title_and_markdown_follow_lifecycle() {
        let progress = Arc::new(RecordingProgress::default());
        let aggregator = InnerProgressAggregator::new(progress.clone());

        aggregator.started(&started_call("fs_read", "call-0"), &json!({}));
        aggregator.started(&started_call("fs_write", "call-1"), &json!({}));
        aggregator.finished(&fake_entry(0, "fs_read", InnerTraceStatus::Ok, "{}"));
        aggregator.finished(&fake_entry(
            1,
            "fs_write",
            InnerTraceStatus::ToolError,
            r#"{"pattern":"x"}"#,
        ));

        let patches = progress.patches.lock().expect("patch log lock").clone();
        let expected = [
            ("bash: fs_read (0 done, 1 running)", None),
            ("bash: fs_read, fs_write (0 done, 2 running)", None),
            (
                "bash: fs_write (1 done, 1 running)",
                Some("fs_read ok 42B 7ms"),
            ),
            (
                "bash: (2 done, 0 running)",
                Some("fs_read ok 42B 7ms\nfs_write tool_error 42B 7ms {\"pattern\":\"x\"}"),
            ),
        ];
        assert_eq!(patches.len(), expected.len());
        for (index, (patch, (title, markdown))) in patches.iter().zip(expected).enumerate() {
            assert_eq!(patch.title.as_deref(), Some(title), "patch {index}");
            assert_eq!(patch.markdown.as_deref(), markdown, "patch {index}");
        }
    }

    #[test]
    fn aggregator_caps_running_names_at_three() {
        let progress = Arc::new(RecordingProgress::default());
        let aggregator = InnerProgressAggregator::new(progress.clone());
        for index in 0..5 {
            aggregator.started(
                &started_call(&format!("tool_{index}"), &format!("call-{index}")),
                &json!({}),
            );
        }

        let patches = progress.patches.lock().expect("patch log lock").clone();
        assert_eq!(patches.len(), 5);
        let (done, running) = title_counts(patches[4].title.as_deref().expect("title"));
        assert_eq!((done, running), (0, 5));
        assert_eq!(
            patches[4].title.as_deref(),
            Some("bash: tool_0, tool_1, tool_2 +2 (0 done, 5 running)")
        );

        for index in 0..5 {
            aggregator.finished(&fake_entry(
                index,
                &format!("tool_{index}"),
                InnerTraceStatus::Ok,
                "null",
            ));
        }
        let patches = progress.patches.lock().expect("patch log lock").clone();
        assert_eq!(
            patches.last().unwrap().title.as_deref(),
            Some("bash: (5 done, 0 running)")
        );
    }

    #[test]
    fn aggregator_keeps_last_ten_trace_lines() {
        let progress = Arc::new(RecordingProgress::default());
        let aggregator = InnerProgressAggregator::new(progress.clone());
        for index in 0..12 {
            aggregator.started(
                &started_call(&format!("tool_{index}"), &format!("call-{index}")),
                &json!({}),
            );
            aggregator.finished(&fake_entry(
                index,
                &format!("tool_{index}"),
                InnerTraceStatus::Ok,
                "null",
            ));
        }

        let last = progress
            .patches
            .lock()
            .expect("patch log lock")
            .last()
            .cloned()
            .expect("patches recorded");
        let markdown = last.markdown.expect("markdown");
        let lines: Vec<&str> = markdown.split('\n').collect();
        assert_eq!(lines.len(), AGGREGATOR_MAX_TRACE_LINES);
        // Oldest two finished calls dropped from the live view.
        assert_eq!(lines[0], "tool_2 ok 42B 7ms");
        assert_eq!(lines[9], "tool_11 ok 42B 7ms");
    }

    #[test]
    fn aggregator_counts_pre_admission_rejections_as_done() {
        let progress = Arc::new(RecordingProgress::default());
        let aggregator = InnerProgressAggregator::new(progress.clone());

        // A rejection never had `started`; it must not linger as running.
        aggregator.finished(&fake_entry(0, "fs_readd", InnerTraceStatus::Rejected, "{}"));

        let last = progress
            .patches
            .lock()
            .expect("patch log lock")
            .last()
            .cloned()
            .expect("patch recorded");
        assert_eq!(last.title.as_deref(), Some("bash: (1 done, 0 running)"));
        assert_eq!(last.markdown.as_deref(), Some("fs_readd rejected 42B 7ms"));
    }

    #[tokio::test]
    async fn aggregator_tracks_concurrent_calls_through_real_runs() {
        let log = Arc::new(DispatchLog::default());
        let provider = Arc::new(FakeToolProvider {
            log: Arc::clone(&log),
            handles: None,
            progress_title: None,
            behavior: Behavior::DelayedOk(Duration::from_millis(25), json!("done")),
        });
        let (ctx, _hooks) = outer_context(shared(&provider), HookControl::Continue);
        let executor = executor_with(&["tool_a"], shared(&provider), (2, 100));
        let progress = Arc::new(RecordingProgress::default());
        let aggregator = Arc::new(InnerProgressAggregator::new(
            Arc::clone(&progress) as Arc<dyn ToolProgress>
        ));
        let run = Arc::new(executor.begin_run(&ctx, PARENT_ROUND, Some(aggregator)));
        let abort = create_abort_signal();

        let mut handles = Vec::new();
        for index in 0..8 {
            let run = Arc::clone(&run);
            let abort = Arc::clone(&abort);
            handles.push(tokio::spawn(async move {
                run.call("tool_a", json!({"n": index}), &abort).await
            }));
        }
        for handle in handles {
            assert!(matches!(
                handle.await.expect("call task"),
                InnerOutcome::Ok { .. }
            ));
        }

        let patches = progress.patches.lock().expect("patch log lock").clone();
        assert!(
            patches.len() >= 16,
            "two patches per call (started + finished), got {}",
            patches.len()
        );
        let mut previous_done = 0;
        for patch in &patches {
            let (done, running) = title_counts(patch.title.as_deref().expect("title"));
            assert!(running <= 2, "semaphore cap leaked into the running set");
            assert!(done >= previous_done, "done count must not decrease");
            assert!(done + running <= 8);
            previous_done = done;
        }
        let last = patches.last().unwrap();
        assert_eq!(last.title.as_deref(), Some("bash: (8 done, 0 running)"));
        let lines = last
            .markdown
            .as_deref()
            .expect("markdown")
            .split('\n')
            .count();
        assert_eq!(lines, 8, "fewer than the line cap, so all calls show");
    }

    #[test]
    fn trace_progress_line_omits_empty_args() {
        assert_eq!(
            fake_entry(0, "fs_read", InnerTraceStatus::Ok, "null").to_progress_line(),
            "fs_read ok 42B 7ms"
        );
        assert_eq!(
            fake_entry(0, "fs_read", InnerTraceStatus::Ok, "{}").to_progress_line(),
            "fs_read ok 42B 7ms"
        );
        assert_eq!(
            fake_entry(0, "fs_read", InnerTraceStatus::Ok, r#"{"path":"a"}"#).to_progress_line(),
            "fs_read ok 42B 7ms {\"path\":\"a\"}"
        );
    }

    #[test]
    fn truncate_bytes_respects_char_boundaries() {
        let text = "é".repeat(10);
        assert_eq!(text.len(), 20);
        let truncated = truncate_bytes(text.clone(), 5);
        assert_eq!(truncated, "éé".to_string());
        assert!(truncated.len() <= 5);
        assert_eq!(truncate_bytes(text.clone(), 20), text);
    }
}
