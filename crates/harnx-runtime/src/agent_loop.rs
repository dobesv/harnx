//! Unified agent loop — `run_agent_loop` is the single canonical
//! implementation of the LLM-call → tool-round → merge → repeat cycle
//! that every front-end (CLI, TUI, and HTTP server) drives through.
//!
//! Previously this logic lived in three places:
//! - `harnx-runtime/src/commands.rs::ask_inner` (CLI)
//! - `harnx-tui/src/prompt.rs::run_prompt_inner` (TUI)
//!
//! Those diverged over time; an older frontend had a bug (#305) where
//! recoverable tool errors ended the session instead of being fed back
//! to the LLM. This module provides the canonical loop that all three
//! front-ends now delegate to.

use crate::{
    config::{Config, GlobalConfig, Input, SessionSaveRequest},
    event_hooks::{run_event_hooks, EventHookInput, EventHookOutcome},
    tool::{execute_tool_round, CompletionText, ToolResult},
    utils::dimmed_text,
};
use anyhow::{bail, Result};
use harnx_core::hooks::HookEventKind;
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use crate::client::retry::call_with_retry_and_fallback;
use crate::client::CompletionTokenUsage;
use crate::tool::ToolCall;
use crate::utils::AbortSignal;

/// Type alias for a custom LLM call function.
///
/// The TUI uses this to inject its streaming path
/// (`call_with_retry_and_fallback_custom` with streaming).
/// The default (when `call_fn` is `None`) is the non-streaming
/// `call_with_retry_and_fallback`.
pub type AgentCallFn = Arc<
    dyn for<'a> Fn(
            &'a mut Input,
            &'a GlobalConfig,
            AbortSignal,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<(
                            String,
                            Option<String>,
                            Vec<ToolCall>,
                            CompletionTokenUsage,
                        )>,
                    > + Send
                    + 'a,
            >,
        > + Send
        + Sync,
>;

/// Callback called after each tool round, before the loop continues.
///
/// Receives the merged `Input` (with tool results already merged in) by
/// mutable reference so the TUI can inject a pending user message into it.
/// Also receives the raw `tool_results` for event emission.
///
/// The TUI uses this to:
/// - Emit `TuiEvent::ToolRoundComplete`
/// - Inject pending user messages into the merged input
/// - Emit `TuiEvent::PendingMessageConsumed`
pub type OnToolRoundFn = Arc<
    dyn for<'a> Fn(
            &'a mut Input,
            &'a [ToolResult],
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>
        + Send
        + Sync,
>;

/// Callback called when the loop ends with a text-only response (no tool
/// calls). The TUI uses this to emit `TuiEvent::Agent(ModelEvent::Final)`.
pub type OnTextResponseFn = Arc<
    dyn Fn(String, CompletionTokenUsage) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

/// Context for running the unified agent loop.
///
/// Construct one and pass to [`run_agent_loop`]. All fields are `Send` so
/// the loop can be called from any async context, including from within an
/// frontend `LocalSet`.
pub struct AgentLoopContext {
    pub config: GlobalConfig,
    pub instance_id: harnx_core::instance::ServerScope,
    pub abort_signal: AbortSignal,
    pub token_budget: Option<u64>,
    pub usage_at_start: CompletionTokenUsage,
    /// Optional custom LLM call function. `None` → uses the default
    /// non-streaming `call_with_retry_and_fallback`.
    pub call_fn: Option<AgentCallFn>,
    /// Optional callback after each tool round. TUI uses this to emit
    /// `ToolRoundComplete` and inject pending messages.
    pub on_tool_round: Option<OnToolRoundFn>,
    /// Optional callback on text-only turn end. TUI uses this to emit
    /// `ModelEvent::Final`.
    pub on_text_response: Option<OnTextResponseFn>,
    /// Preserve old CLI ask_inner behavior for status-line prefix and auto-resume.
    pub initial_with_embeddings: bool,
    pub initial_resume_count: u32,
    pub max_resume: Option<u32>,
    /// Runtime-owned, lazy handoff identity preparation before result persistence.
    pub prepare_handoff_fn: Option<Arc<crate::tool::PrepareHandoffFn>>,

    pub pending_async_context: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    /// Mid-turn `note`-visibility surface actions, drained into
    /// `Input::hook_context` at the next model call. Deliberately separate
    /// from `pending_async_context`: that queue also feeds the turn-end
    /// resume path, which would turn a passive note into a new user prompt
    /// (and so an extra turn) instead of passive context.
    pub surface_notes: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    /// Optional per-session working directory. When unset, runtime falls back
    /// to process cwd for CLI compatibility.
    pub working_dir: Option<PathBuf>,
    /// Repetition guard for this turn's tool loop. Frontends build a new
    /// context for every turn, so every turn starts with an empty guard.
    pub tool_loop_guard: crate::tool_loop_guard::ToolLoopGuardHandle,
}

impl AgentLoopContext {
    /// The session log is the only interrupt authority; the abort signal is
    /// this process's view of the `Cancel` its watcher already saw. Every
    /// former gate boundary is now just this check.
    pub(crate) fn check_generation(&self, boundary: &str) -> Result<()> {
        anyhow::ensure!(!self.abort_signal.aborted(), "interrupted at {boundary}");
        if self
            .config
            .read()
            .run_context
            .as_ref()
            .is_some_and(|record| record.is_expired_at(chrono::Utc::now()))
        {
            return Err(crate::nats_session_metadata::run_limits::DeadlineExpired.into());
        }
        Ok(())
    }
}

// qual:allow(coupling, deh) reason: "Engine suspension crosses the anyhow tool boundary; distinguish it from provider failures."
async fn defer_tool_approval(ctx: &AgentLoopContext, error: anyhow::Error) -> Result<LoopResult> {
    ctx.check_generation("completion-error")?;
    let Some(suspended) = error.downcast_ref::<harnx_engine::tool::ToolInputRequiredError>() else {
        return Err(error);
    };
    let tool_call_id = suspended
        .0
        .pending
        .first()
        .and_then(|p| p.call.id.clone())
        .ok_or_else(|| anyhow::anyhow!("durable suspension has no pending call"))?;
    Ok(LoopResult::AwaitingHitlApproval { tool_call_id })
}

/// Resume a tool round that was interrupted for approval.
///
/// This is the continuation seam for Design B interrupt/resume:
/// - Takes the interrupted round: its `ToolCalls` entry's sequence, the saved
///   assistant output/thought, and the calls that were deferred
/// - Takes resolved approve/deny decisions for each call
/// - Executes approved calls and supplies synthetic results for saved denials
/// - Persists ToolResults via existing session helpers
/// - Continues the canonical loop from the post-tool state
///
/// `pending_interrupt_ids` must contain only tool-call ids that still have an
/// unanswered `HitlApprovalRequested` entry for this round: an id with no
/// decision parks the round as still awaiting approval. This entry point also
/// serves middleware-interaction recovery, whose calls have no legacy request —
/// passing every round call id would park those resumes forever.
pub async fn continue_agent_loop_from_tool_round(
    ctx: &AgentLoopContext,
    mut input: Input,
    round: PendingToolRound,
    decisions: Vec<ToolApprovalDecision>,
    pending_interrupt_ids: std::collections::BTreeSet<String>,
) -> Result<LoopResult> {
    let config = &ctx.config;

    // Old approvals are transcript authority even though process hooks no
    // longer run. Do not let removing the hook turn a saved denial into dispatch.
    if let Some(tool_call_id) = pending_interrupt_ids.iter().find(|id| {
        !decisions
            .iter()
            .any(|decision| decision.tool_call_id == **id)
    }) {
        return Ok(LoopResult::AwaitingHitlApproval {
            tool_call_id: tool_call_id.clone(),
        });
    }
    let denied = round
        .calls
        .iter()
        .filter_map(|call| {
            let decision = decisions.iter().find(|decision| {
                call.id.as_deref() == Some(&decision.tool_call_id) && !decision.approved
            })?;
            Some(ToolResult::new(
                call.clone(),
                serde_json::json!({
                    "error": decision.reason.as_deref().unwrap_or("Denied by user"),
                    "blocked_by_hook": true,
                    "is_error": true,
                }),
            ))
        })
        .collect();
    let tool_round = Box::pin(crate::tool::execute_tool_round_with_persistence(
        ctx.tool_round_params(
            config,
            &input,
            CompletionText {
                output: &round.output,
                thought: round.thought.as_deref(),
            },
        ),
        round.calls,
        crate::tool::ToolRoundPersistence::LegacyApprovals {
            seq: round.seq,
            denied,
        },
    ))
    .await;
    let tool_results = match tool_round {
        Ok(results) => results,
        Err(error) => return defer_tool_approval(ctx, error).await,
    };

    // Merge tool results into input for the next round
    if !tool_results.is_empty() {
        let switch_agent = crate::handoff_result::select(&tool_results);
        let mut merged_input =
            input.merge_tool_results(round.output, round.thought, tool_results.clone());

        // Invoke on_tool_round callback
        if let Some(ref cb) = ctx.on_tool_round {
            cb(&mut merged_input, &tool_results).await?;
        }

        if let Some(switch) = switch_agent {
            emit_handoff_request(ctx, &switch);
            return Ok(LoopResult::HandoffRequested {
                agent: switch.agent,
                session_id: switch.session_id,
                prompt: switch.prompt,
                tool_call_id: switch.tool_call_id,
            });
        }

        input = merged_input;
    }

    // Continue the canonical loop from post-tool state
    run_agent_loop(ctx, input).await
}

/// A tool round the session log holds without its results: the sequence of
/// its `ToolCalls` entry and what that entry recorded.
#[derive(Debug, Clone)]
pub struct PendingToolRound {
    /// Sequence of the round's `ToolCalls` entry, which its calls are
    /// journaled under.
    pub seq: u64,
    pub output: String,
    pub thought: Option<String>,
    pub calls: Vec<ToolCall>,
}

/// A resolved approval decision for a single pending tool call.
#[derive(Debug, Clone)]
pub struct ToolApprovalDecision {
    /// The tool call ID being resolved.
    pub tool_call_id: String,
    /// Whether the call was approved.
    pub approved: bool,
    /// Optional reason for denial.
    pub reason: Option<String>,
}

pub enum LoopResult {
    Completed,
    AwaitingHitlApproval {
        tool_call_id: String,
    },
    HandoffRequested {
        agent: String,
        session_id: Option<String>,
        prompt: String,
        tool_call_id: Option<String>,
    },
}

/// Run the canonical agent loop.
///
/// Executes: embeddings → async-hook drain → `before_chat_completion` →
/// `UserPromptSubmit` hook → LLM call (with retry/fallback) → tool round
/// (if tool calls) → persist → stop hook → resume / agent switch / done.
/// Repeats until no tool results and no resume signal.
///
/// On clean exit returns `Ok(LoopResult::Completed)`. On LLM error dispatches
/// `StopFailure` hook and propagates. On fatal tool error propagates.
/// Recoverable tool errors are already converted to `{"is_error":true}`
/// results by `execute_tool_round` and fed back to the LLM.
pub async fn run_agent_loop(ctx: &AgentLoopContext, initial_input: Input) -> Result<LoopResult> {
    if initial_input.is_empty() {
        return Box::pin(run_agent_loop_inner(ctx, initial_input)).await;
    }

    with_turn_lifecycle(ctx, Box::pin(run_agent_loop_inner(ctx, initial_input))).await
}

/// Run one turn while allowing a caller to commit control-plane work before
/// the terminal [`TurnEvent::Ended`] advisory is emitted.
///
/// NATS handoffs use this seam to durably queue and activate the target, then
/// publish their committed destination while the source turn is still active.
pub(crate) async fn run_agent_loop_with_before_end<F, Fut>(
    ctx: &AgentLoopContext,
    initial_input: Input,
    before_end: F,
) -> Result<LoopResult>
where
    F: FnOnce(&LoopResult) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if initial_input.is_empty() {
        let result = Box::pin(run_agent_loop_inner(ctx, initial_input)).await?;
        before_end(&result).await?;
        return Ok(result);
    }

    emit_turn_started();
    let result = async {
        let result = Box::pin(run_agent_loop_inner(ctx, initial_input)).await?;
        before_end(&result).await?;
        Ok(result)
    }
    .await;
    emit_turn_ended(ctx, &result).await?;
    result
}

async fn with_turn_lifecycle<T>(
    ctx: &AgentLoopContext,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    emit_turn_started();
    let result = future.await;
    emit_turn_ended(ctx, &result).await?;
    result
}

fn emit_turn_started() {
    use harnx_core::event::{AgentEvent, TurnEvent};

    harnx_core::sink::emit_agent_event(AgentEvent::Turn(TurnEvent::Started));
}

async fn emit_turn_ended<T>(ctx: &AgentLoopContext, result: &Result<T>) -> Result<()> {
    use harnx_core::event::{AgentEvent, ModelEvent, TurnEvent, TurnOutcome};

    if ctx.abort_signal.aborted() {
        return Ok(());
    }
    ctx.check_generation("turn-ended")?;
    if let Err(error) = result {
        if !ctx.abort_signal.aborted() {
            // The persisted error keeps the machine-readable marker for
            // parents; people watching live only need the sentence.
            let text = harnx_core::loop_guard::repetition_stop_sentence(error)
                .unwrap_or_else(|| harnx_render::pretty_error_string(error));
            harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::Error(text)));
        }
    }
    harnx_core::sink::emit_agent_event(AgentEvent::Turn(TurnEvent::Ended {
        outcome: TurnOutcome::default(),
    }));
    Ok(())
}

/// Runs agent loop, applying file-backed local handoffs until completion.
///
/// Retained for test-only in-process frontend executors. Production frontends
/// route turns through NATS; workers use [`run_agent_loop`] directly with
/// NATS-backed handoff handling.
pub async fn run_agent_loop_with_local_handoff(
    ctx: &AgentLoopContext,
    mut input: Input,
) -> Result<()> {
    if input.is_empty() {
        return Box::pin(run_agent_loop_inner(ctx, input)).await.map(|_| ());
    }

    with_turn_lifecycle(ctx, async move {
        loop {
            match Box::pin(run_agent_loop_inner(ctx, input)).await? {
                LoopResult::Completed | LoopResult::AwaitingHitlApproval { .. } => return Ok(()),
                LoopResult::HandoffRequested {
                    agent,
                    session_id,
                    prompt,
                    tool_call_id: _,
                } => {
                    apply_local_handoff(ctx, &agent, session_id.as_deref(), &prompt).await?;
                    input = crate::config::input::from_str(&ctx.config, &prompt, None);
                }
            }
        }
    })
    .await
}

async fn apply_local_handoff(
    ctx: &AgentLoopContext,
    agent: &str,
    session_id: Option<&str>,
    _prompt: &str,
) -> Result<()> {
    ctx.config.write().exit_agent()?;
    Config::use_agent(&ctx.config, agent, session_id, ctx.abort_signal.clone()).await?;
    Ok(())
}

async fn inject_shared_pending_context(
    input: &mut Input,
    shared_pending: Option<&Arc<tokio::sync::Mutex<Option<String>>>>,
) {
    if let Some(shared_pending) = shared_pending {
        let pending = shared_pending.lock().await.take();
        if let Some(context) = pending.filter(|value| !value.is_empty()) {
            // Hook-added context has no other durable copy, so it rides
            // `hook_context` — durably appended as its own user message even
            // on tool continuations and worker turns (issue #2198). It used to
            // be prepended to `Input.text`, which mid-turn and worker rounds
            // dropped on the floor.
            input.push_hook_context(&context);
        }
    }
}

struct TurnHookContext {
    session_id: String,
    cwd: PathBuf,
    max_resume: u32,
}

fn turn_hook_context(ctx: &AgentLoopContext) -> TurnHookContext {
    let config = ctx.config.read();
    let hooks = config.resolved_hooks();
    TurnHookContext {
        session_id: config
            .session
            .as_ref()
            .map(|session| session.id().to_string())
            .unwrap_or_else(|| "default".to_string()),
        cwd: ctx
            .working_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
        max_resume: ctx
            .max_resume
            .unwrap_or_else(|| hooks.max_resume.unwrap_or(5)),
    }
}

async fn wait_for_session_compaction(config: &GlobalConfig) {
    loop {
        // Polled without waiting on the lock, whose holder may be the very
        // compaction this waits for. A held lock usually frees in
        // microseconds, so look again sooner than a running compaction
        // would warrant.
        let compacting = config.try_read().map(|guard| guard.is_compacting_session());
        let delay = match compacting {
            Some(false) => return,
            Some(true) => 100,
            None => 1,
        };
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
}

async fn apply_round_embeddings(
    input: &mut Input,
    config: &GlobalConfig,
    abort_signal: &AbortSignal,
    enabled: bool,
) -> Result<()> {
    if enabled {
        crate::config::input::use_embeddings(input, config, abort_signal.clone()).await?;
    }
    Ok(())
}

/// The UserPromptSubmit chain for fresh prompts: `block` rejects the turn
/// before anything is persisted, `rewrite_prompt` replaces the prompt the
/// model sees, and context rides `hook_context` into the request. A `deny`
/// failure blocks with the error naming the hook. Tool-round continuations
/// re-enter with `tool_calls` set and skip this chain; a crash-resumed,
/// orphan-repaired tool round rebuilds its input without tool calls
/// (`derive_resumable_turn_input`), so its chain re-runs — a known gap.
// qual:allow(iosp) — one seam gates fresh prompts and folds the hook chain's block/rewrite/context effects onto the input; splitting it would re-thread the outcome.
async fn apply_user_prompt_hooks(
    ctx: &AgentLoopContext,
    input: &mut Input,
    turn: &TurnHookContext,
    resume_count: u32,
) -> Result<bool> {
    if input.tool_calls.is_some() || input.is_empty() {
        return Ok(true);
    }
    let outcome = run_event_hooks(
        &ctx.config,
        &EventHookInput {
            event: HookEventKind::UserPromptSubmit,
            session_id: turn.session_id.clone(),
            cwd: turn.cwd.clone(),
            resume_count,
            prompt: Some(input.text().to_string()),
            ..Default::default()
        },
        &ctx.abort_signal,
    )
    .await;
    match outcome {
        Ok(outcome) => {
            if let Some(reason) = outcome.block {
                emit_user_prompt_block_notice(reason);
                return Ok(false);
            }
            if let Some(prompt) = outcome.rewrite.clone() {
                apply_prompt_rewrite(ctx, input, &prompt);
            }
            if let Some(context) = outcome.context_text() {
                input.push_hook_context(&context);
            }
            Ok(true)
        }
        Err(error) => {
            emit_user_prompt_block_notice(format!("{error:#}"));
            Ok(false)
        }
    }
}

/// Apply a `rewrite_prompt` to the input, and on worker turns to the durable
/// rows the request is built from. The prompt the hook saw is captured before
/// the rewrite replaces it, so the durable-row matching below can find the
/// original text.
fn apply_prompt_rewrite(ctx: &AgentLoopContext, input: &mut Input, prompt: &str) {
    let original = input.text();
    input.clear_patch();
    input.set_text(prompt.to_string());
    if input.skip_user_log_append {
        rewrite_durable_prompt_rows(ctx, input, &original, prompt);
    }
}

/// A NATS worker folds its turn input out of admitted durable user rows and
/// suppresses re-appending them (`skip_user_log_append`), so request building
/// uses the session's durable rows and `Input::set_text` alone never reaches
/// the model: the turn would answer the original client text. Rewrite the
/// folded rows in place — ids (and therefore their prompt admissions)
/// unchanged — in both the input's selected rows and the in-memory session,
/// so every request this turn builds carries the rewritten prompt without
/// appending a second unanswered user row.
///
/// The durable log keeps the original text: it is append-only, and a worker
/// that re-enters the loop for this prompt re-runs the hook chain, re-deriving
/// the rewrite instead of persisting a duplicate prompt row.
fn rewrite_durable_prompt_rows(
    ctx: &AgentLoopContext,
    input: &mut Input,
    original: &str,
    rewritten: &str,
) {
    // The submitted prompt is the newest folded row; older folded rows are
    // separate queued messages the hook saw only as part of the joined text.
    let selected = input.durable_user_messages.last().cloned();
    let mut session = ctx.config.write();
    let Some(session) = session.session.as_mut() else {
        log::warn!(
            "UserPromptSubmit rewrite has no session to apply to; the model sees the original prompt"
        );
        return;
    };
    let mut rewritten_any = false;
    for message in session.messages.iter_mut() {
        // `same_identity` matches the selected row's id wherever it appears —
        // the live row and any compaction copy — so a later compaction re-log
        // carries the rewrite too, and it cannot bleed into unrelated rows.
        let matches = match &selected {
            Some(selected) => message.same_identity(selected),
            // No selected rows (e.g. a resumed turn): without an identity to
            // pin the rewrite to, every user row carrying the exact text the
            // hook rewrote matches — older duplicates with the same text are
            // rewritten too.
            None => message.role.is_user() && message.content.to_text() == original,
        };
        if matches {
            message.content = harnx_core::message::MessageContent::Text(rewritten.to_string());
            rewritten_any = true;
        }
    }
    if let Some(last) = input.durable_user_messages.last_mut() {
        last.content = harnx_core::message::MessageContent::Text(rewritten.to_string());
    }
    if !rewritten_any {
        log::warn!(
            "UserPromptSubmit rewrite found no durable row for the original prompt; \
             the model may see the original text"
        );
    }
}

async fn pre_model_call_boundary_passes(ctx: &AgentLoopContext) -> Result<bool> {
    ctx.check_generation("model-handoff")?;
    enforce_token_budget(ctx)?;
    Ok(true)
}

fn emit_user_prompt_block_notice(reason: String) {
    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Notice(
        harnx_core::event::NoticeEvent::Error(reason),
    ));
}

type AgentModelResult = Result<(String, Option<String>, Vec<ToolCall>, CompletionTokenUsage)>;

async fn call_agent_model(ctx: &AgentLoopContext, input: &mut Input) -> AgentModelResult {
    crate::tool_context::refresh_nats_tool_declarations(&ctx.config, &ctx.instance_id).await;
    if let Some(call_fn) = &ctx.call_fn {
        call_fn(input, &ctx.config, ctx.abort_signal.clone()).await
    } else {
        call_with_retry_and_fallback(input, &ctx.config, ctx.abort_signal.clone()).await
    }
}

async fn committed_agent_model(ctx: &AgentLoopContext, input: &mut Input) -> AgentModelResult {
    let result = call_agent_model(ctx, input).await;
    // The response can win the local select after the interrupt landed.
    ctx.check_generation("model-resolved")?;
    result
}

struct FailedModelTurn<'a> {
    ctx: &'a AgentLoopContext,
    input: &'a Input,
    turn: &'a TurnHookContext,
    resume_count: u32,
    error: anyhow::Error,
}

async fn fail_model_turn(params: FailedModelTurn<'_>) -> Result<LoopResult> {
    let FailedModelTurn {
        ctx,
        input,
        turn,
        resume_count,
        error,
    } = params;
    // Remote cancellation is already represented by its durable Cancel entry.
    // Persisting an empty assistant response here would come after that entry
    // and incorrectly make reconstruction treat the cancelled turn as idle.
    if ctx.abort_signal.aborted() {
        return Err(error);
    }
    ctx.check_generation("model-failure")?;
    // StopFailure hooks only observe. `deny` is a config error on this event,
    // so failures are logged and the turn still fails with the model error.
    let outcome = run_event_hooks(
        &ctx.config,
        &EventHookInput {
            event: HookEventKind::StopFailure,
            session_id: turn.session_id.clone(),
            cwd: turn.cwd.clone(),
            resume_count,
            error: Some(error.to_string()),
            error_type: Some("api_error".to_string()),
            ..Default::default()
        },
        &ctx.abort_signal,
    )
    .await;
    if let Err(hook_error) = outcome {
        log::warn!("StopFailure event hooks skipped: {hook_error:#}");
    }
    let request = SessionSaveRequest::new(input, "", None);
    let persistence = {
        ctx.config
            .write()
            .prepare_after_chat_completion(&request, &[], &Default::default())
    };
    if let Ok(persistence) = persistence {
        persistence.persist().await;
    }
    Err(error)
}

struct CompletionOutput<'a> {
    output: &'a str,
    thought: Option<&'a str>,
    tool_calls: Vec<ToolCall>,
    usage: &'a CompletionTokenUsage,
}

async fn complete_model_turn(
    ctx: &AgentLoopContext,
    input: &Input,
    completion: CompletionOutput<'_>,
) -> Result<Vec<ToolResult>> {
    if completion.tool_calls.is_empty() {
        let request = SessionSaveRequest::new(input, completion.output, completion.thought);
        let persistence = {
            ctx.config
                .write()
                .prepare_after_chat_completion(&request, &[], completion.usage)?
        };
        persistence.persist().await;
        return Ok(Vec::new());
    }
    ctx.config.write().record_completion_usage(completion.usage);
    execute_tool_round(
        ctx.tool_round_params(
            &ctx.config,
            input,
            CompletionText {
                output: completion.output,
                thought: completion.thought,
            },
        ),
        completion.tool_calls,
    )
    .await
}

async fn emit_final_text_response(
    ctx: &AgentLoopContext,
    output: String,
    usage: CompletionTokenUsage,
) {
    if let Some(callback) = &ctx.on_text_response {
        callback(output, usage).await;
    } else {
        harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Model(
            harnx_core::event::ModelEvent::Final { output, usage },
        ));
    }
}

fn emit_text_turn_status(
    ctx: &AgentLoopContext,
    usage: &CompletionTokenUsage,
    text_only: bool,
    emitted_text_turns: u32,
) {
    if ctx.on_text_response.is_some() || !text_only {
        return;
    }
    let config = ctx.config.read();
    let macro_flag = config.macro_flag;
    let status = config.render_status_line(true);
    let session_usage = config
        .session
        .as_ref()
        .map(|session| session.completion_usage().clone());
    let display_usage = session_usage.as_ref().unwrap_or(usage);
    let context_stats = config
        .session
        .as_ref()
        .map(|session| format_context_stats(session.tokens_usage()))
        .unwrap_or_default();
    drop(config);

    let mut line_parts = Vec::new();
    push_status_part(&mut line_parts, status);
    push_status_part(
        &mut line_parts,
        (!display_usage.is_empty()).then(|| format!("   {display_usage}")),
    );
    push_status_part(
        &mut line_parts,
        (!context_stats.is_empty()).then(|| format!("  {context_stats}")),
    );
    emit_status_parts(line_parts, macro_flag, emitted_text_turns);
}

fn format_context_stats((tokens, percent): (usize, f32)) -> String {
    if percent > 0.0 {
        format!("💬 {}({:.0}%)", tokens, percent)
    } else {
        format!("💬 {tokens}")
    }
}

fn push_status_part(parts: &mut Vec<String>, part: impl Into<Option<String>>) {
    if let Some(part) = part.into().filter(|value| !value.is_empty()) {
        parts.push(part);
    }
}

fn emit_status_parts(parts: Vec<String>, macro_flag: bool, emitted_text_turns: u32) {
    if parts.is_empty() {
        return;
    }
    let prefix = if macro_flag || emitted_text_turns == 0 {
        ""
    } else {
        "\n"
    };
    crate::utils::emit_info(format!("{prefix}{}", dimmed_text(&parts.join(""))));
}

struct TextStopDispatch<'a> {
    ctx: &'a AgentLoopContext,
    turn: &'a TurnHookContext,
    resume_count: u32,
    output: &'a str,
    has_tool_results: bool,
}

// qual:allow(iosp) — assembles the Stop event input and maps the single run result; the input fields and the error mapping belong to the same seam.
async fn dispatch_text_stop(params: TextStopDispatch<'_>) -> Option<EventHookOutcome> {
    if params.has_tool_results {
        return None;
    }
    let outcome = run_event_hooks(
        &params.ctx.config,
        &EventHookInput {
            event: HookEventKind::Stop,
            session_id: params.turn.session_id.clone(),
            cwd: params.turn.cwd.clone(),
            resume_count: params.resume_count,
            last_assistant_message: Some(params.output.to_string()),
            ..Default::default()
        },
        &params.ctx.abort_signal,
    )
    .await;
    match outcome {
        Ok(outcome) => Some(outcome),
        Err(error) => {
            // Stop hooks are allow-only by construction (deny is a config
            // error), so this is a resolution failure, not a hook failure.
            log::warn!("Stop event hooks skipped: {error:#}");
            None
        }
    }
}

struct ToolRoundOutput {
    output: String,
    thought: Option<String>,
    tool_results: Vec<ToolResult>,
}

enum ToolRoundAdvance {
    Continue(Box<Input>),
    Handoff(LoopResult),
}

async fn advance_tool_round(
    ctx: &AgentLoopContext,
    input: Input,
    round: ToolRoundOutput,
) -> Result<ToolRoundAdvance> {
    ctx.check_generation("tool-round-reducer")?;
    let switch_agent = crate::handoff_result::select(&round.tool_results);
    let mut merged_input =
        input.merge_tool_results(round.output, round.thought, round.tool_results.clone());
    if let Some(callback) = &ctx.on_tool_round {
        callback(&mut merged_input, &round.tool_results).await?;
    }
    if let Some(switch) = switch_agent {
        emit_handoff_request(ctx, &switch);
        return Ok(ToolRoundAdvance::Handoff(LoopResult::HandoffRequested {
            agent: switch.agent,
            session_id: switch.session_id,
            prompt: switch.prompt,
            tool_call_id: switch.tool_call_id,
        }));
    }
    // Mid-loop title generation fires after each tool round on the continue path.
    // Kept out of run_agent_loop_inner to avoid adding LoC to that already-complex method.
    Config::maybe_generate_title_mid_loop(Arc::clone(&ctx.config));
    Ok(ToolRoundAdvance::Continue(Box::new(merged_input)))
}

fn emit_handoff_request(ctx: &AgentLoopContext, switch: &harnx_core::tool::SwitchAgentData) {
    let _ = ctx;
    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Turn(
        harnx_core::event::TurnEvent::HandoffRequested {
            agent: switch.agent.clone(),
            session_id: switch
                .session_id
                .clone()
                .filter(|session_id| !session_id.trim().is_empty()),
        },
    ));
}

enum ResumeAction {
    None,
    Abort,
    Context(String),
}

fn stop_resume_action(
    ctx: &AgentLoopContext,
    turn: &TurnHookContext,
    resume_count: u32,
    outcome: Option<EventHookOutcome>,
) -> ResumeAction {
    let Some(outcome) = outcome else {
        return ResumeAction::None;
    };
    // First resume wins: the runner stops the chain after the first hook that
    // called `resume`, so `outcome.resume` is already the winning message.
    let Some(message) = outcome.resume else {
        return ResumeAction::None;
    };
    if resume_count >= turn.max_resume {
        return ResumeAction::None;
    }
    if ctx.abort_signal.aborted() {
        return ResumeAction::Abort;
    }
    ResumeAction::Context(if message.trim().is_empty() {
        "Continue working on pending tasks.".to_string()
    } else {
        message
    })
}

async fn pending_resume_action(
    ctx: &AgentLoopContext,
    turn: &TurnHookContext,
    resume_count: u32,
) -> ResumeAction {
    if resume_count >= turn.max_resume {
        return ResumeAction::None;
    }
    let Some(shared_pending) = &ctx.pending_async_context else {
        return ResumeAction::None;
    };
    let Some(context) = shared_pending
        .lock()
        .await
        .take()
        .filter(|value| !value.is_empty())
    else {
        return ResumeAction::None;
    };
    if ctx.abort_signal.aborted() {
        ResumeAction::Abort
    } else {
        ResumeAction::Context(context)
    }
}

fn record_agent_turn_attributes(ctx: &AgentLoopContext) {
    let span = tracing::Span::current();
    if !span.is_disabled() {
        let config = ctx.config.read();
        if let Some(session) = config.session.as_ref() {
            span.record("harnx.session.id", session.id());
        }
        let agent_name = config.agent.as_ref().map(|agent| agent.name()).or_else(|| {
            config
                .session
                .as_ref()
                .and_then(|session| session.agent_name.as_deref())
        });
        if let Some(agent_name) = agent_name {
            span.record("harnx.agent.name", agent_name);
        }
    }
}

fn finish_agent_loop(config: &GlobalConfig, abort_signal: &AbortSignal) -> Result<LoopResult> {
    if abort_signal.aborted() {
        bail!("interrupted by user");
    }
    Config::run_post_turn_maintenance(Arc::clone(config));
    Ok(LoopResult::Completed)
}

fn enforce_token_budget(ctx: &AgentLoopContext) -> Result<()> {
    let Some(budget) = ctx.token_budget.filter(|budget| *budget > 0) else {
        return Ok(());
    };
    let current_budgeted = ctx
        .config
        .read()
        .session
        .as_ref()
        .map(|session| session.completion_usage().budgeted_tokens())
        .unwrap_or_default();
    let budgeted = current_budgeted.saturating_sub(ctx.usage_at_start.budgeted_tokens());

    if budgeted >= budget {
        return Err(anyhow::Error::new(
            crate::terminated_result::TokenBudgetExceeded::new(budgeted, budget),
        ));
    }
    Ok(())
}

mod compaction;
mod overflow;

async fn prepare_round_input(
    ctx: &AgentLoopContext,
    input: &mut Input,
    with_embeddings: bool,
) -> Result<()> {
    wait_for_session_compaction(&ctx.config).await;
    apply_round_embeddings(input, &ctx.config, &ctx.abort_signal, with_embeddings).await?;
    inject_shared_pending_context(input, ctx.pending_async_context.as_ref()).await;
    inject_shared_pending_context(input, ctx.surface_notes.as_ref()).await;
    compaction::preflight(ctx, input).await?;
    ctx.check_generation("before-model")?;
    ctx.config.write().before_chat_completion(input)?;
    crate::config::session_timing::prepare_notice(&ctx.config, input, chrono::Utc::now())?;
    ctx.check_generation("timing-persisted")
}

fn begin_agent_loop(ctx: &AgentLoopContext, mut input: Input) -> Input {
    record_agent_turn_attributes(ctx);
    // A logical loop entry must not replay a saved request-only suffix.
    input.timing_context = None;
    crate::config::session_timing::start_run(&ctx.config, chrono::Utc::now());
    input
}

fn clear_persisted_round_context(input: &mut Input) {
    // Input was persisted before the model call. Clear one-shot context before
    // on_tool_round supplies the next pending user message.
    input.injected_user_text = None;
    input.injected_user_messages.clear();
    input.hook_context = None;
    input.timing_context = None;
}

#[tracing::instrument(
    name = "agent_turn",
    skip_all,
    fields(
        otel.kind = "internal",
        harnx.session.id = tracing::field::Empty,
        harnx.agent.name = tracing::field::Empty,
    )
)]
// qual:allow(complexity, max_cognitive=19) reason: "Lifecycle dispatch (Stop/StopFailure hooks, resume bounds, per-round hook context) lives in the loop body beside the turn state machine it drives; hoisting it out would re-thread the mutable input and resume counters."
// qual:allow(complexity, max_cyclomatic=12) reason: "Branches map one-to-one to turn states (resume, block, failure) plus the Stop-hook resume check; a match-heavy refactor would hide the state order."
// qual:allow(complexity, max_function_lines=129) reason: "One function owns one durable turn: prompt preparation, model rounds, event-hook seams and terminal handling read as a sequence; extracting sub-steps splits the shared Input state."
async fn run_agent_loop_inner(ctx: &AgentLoopContext, initial_input: Input) -> Result<LoopResult> {
    let config = &ctx.config;
    let abort_signal = &ctx.abort_signal;
    let mut input = begin_agent_loop(ctx, initial_input);
    let mut resume_count: u32 = ctx.initial_resume_count;
    let mut with_embeddings = ctx.initial_with_embeddings;
    let mut emitted_text_turns: u32 = 0;
    let mut turn_usage = CompletionTokenUsage::default();

    let turn = turn_hook_context(ctx);
    // Hook block/rewrite effects happen before input preparation.
    if !apply_user_prompt_hooks(ctx, &mut input, &turn, resume_count).await? {
        ctx.check_generation("loop-completed")?;
        return finish_agent_loop(config, abort_signal);
    }

    loop {
        if input.is_empty() {
            break;
        }

        prepare_round_input(ctx, &mut input, with_embeddings).await?;

        if !pre_model_call_boundary_passes(ctx).await? {
            break;
        }

        let llm_result = Box::pin(overflow::call(ctx, &mut input)).await;

        let (output, thought, tool_calls, usage) = match llm_result {
            Ok(result) => result,
            Err(err) => {
                return fail_model_turn(FailedModelTurn {
                    ctx,
                    input: &input,
                    turn: &turn,
                    resume_count,
                    error: err,
                })
                .await;
            }
        };
        turn_usage.accumulate(&usage);

        let tool_results = match complete_model_turn(
            ctx,
            &input,
            CompletionOutput {
                output: &output,
                thought: thought.as_deref(),
                tool_calls,
                usage: &usage,
            },
        )
        .await
        {
            Ok(results) => results,
            Err(error) => return defer_tool_approval(ctx, error).await,
        };

        ctx.check_generation("completion-reducer")?;
        clear_persisted_round_context(&mut input);

        emit_text_turn_status(ctx, &usage, tool_results.is_empty(), emitted_text_turns);

        let stop_outcome = dispatch_text_stop(TextStopDispatch {
            ctx,
            turn: &turn,
            resume_count,
            output: &output,
            has_tool_results: !tool_results.is_empty(),
        })
        .await;

        if !tool_results.is_empty() {
            match advance_tool_round(
                ctx,
                input,
                ToolRoundOutput {
                    output,
                    thought,
                    tool_results,
                },
            )
            .await?
            {
                ToolRoundAdvance::Continue(next_input) => input = *next_input,
                ToolRoundAdvance::Handoff(result) => return Ok(result),
            }
            with_embeddings = false;
            continue;
        }

        emitted_text_turns += 1;

        match stop_resume_action(ctx, &turn, resume_count, stop_outcome) {
            ResumeAction::Context(context) => {
                input = crate::config::input::from_str(config, &context, None);
                resume_count += 1;
                with_embeddings = true;
                continue;
            }
            ResumeAction::Abort => break,
            ResumeAction::None => {}
        }

        match pending_resume_action(ctx, &turn, resume_count).await {
            ResumeAction::Context(context) => {
                input = crate::config::input::from_str(config, &context, None);
                resume_count += 1;
                with_embeddings = true;
                continue;
            }
            ResumeAction::Abort => break,
            ResumeAction::None => {}
        }

        ctx.check_generation("model-final")?;
        emit_final_text_response(ctx, output, turn_usage).await;

        // Done.
        break;
    }

    ctx.check_generation("loop-completed")?;
    finish_agent_loop(config, abort_signal)
}

#[cfg(test)]
mod tests {
    mod compaction_tests;
    mod event_hook_loop_tests;
    mod future_size_tests;
    mod input_persistence_tests;
    mod overflow_tests;
    mod preflight_failure_tests;
    mod tool_loop_guard_tests;

    use super::*;
    use crate::client::{
        ChatCompletionsOutput, ClientConfig, MessageRole, Model, ModelData, TestStateGuard,
    };
    use crate::config::ConfigLock;
    use crate::test_utils::{MockClient, MockTurn, MockTurnBuilder};
    use crate::utils::create_abort_signal;
    use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, NoticeEvent, TurnEvent};
    use metrics::{Key, Label};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey, MetricKind,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    static SINK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    type DebugMetric = (
        CompositeKey,
        Option<metrics::Unit>,
        Option<metrics::SharedString>,
        DebugValue,
    );

    fn metric_key(kind: MetricKind, name: &'static str, labels: &[(&str, &str)]) -> CompositeKey {
        CompositeKey::new(
            kind,
            Key::from_parts(
                name,
                labels
                    .iter()
                    .map(|(key, value)| Label::new((*key).to_owned(), (*value).to_owned()))
                    .collect::<Vec<_>>(),
            ),
        )
    }

    fn metric_value<'a>(
        snapshot: &'a [DebugMetric],
        kind: MetricKind,
        name: &'static str,
        labels: &[(&str, &str)],
    ) -> &'a DebugValue {
        let expected_key = metric_key(kind, name, labels);
        snapshot
            .iter()
            .find(|(key, _, _, _)| key == &expected_key)
            .map(|(_, _, _, value)| value)
            .unwrap_or_else(|| panic!("metric not found: {expected_key:?}"))
    }

    fn token_metric_value<'a>(snapshot: &'a [DebugMetric], usage_type: &str) -> &'a DebugValue {
        metric_value(
            snapshot,
            MetricKind::Counter,
            harnx_metrics::LLM_TOKENS_TOTAL,
            &[
                ("agent", "metrics-agent"),
                ("client", "mypkg/openai"),
                ("model", "metrics-model"),
                ("provider", ""),
                ("type", usage_type),
            ],
        )
    }

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<(AgentEvent, Option<AgentSource>)>>,
    }

    impl AgentEventSink for CollectingSink {
        fn emit(&self, event: AgentEvent) {
            let (event, source) = match event {
                AgentEvent::SubAgent { source, event } => (*event, Some(source)),
                event => (event, None),
            };
            self.events.lock().unwrap().push((event, source));
        }
    }

    fn assert_single_completed_turn(sink: &CollectingSink) {
        let events = sink.events.lock().unwrap();
        assert!(matches!(
            events.first(),
            Some((AgentEvent::Turn(TurnEvent::Started), None))
        ));
        assert!(matches!(
            events.last(),
            Some((
                AgentEvent::Turn(TurnEvent::Ended { .. } | TurnEvent::Interrupted { .. }),
                None
            ))
        ));
        let final_outputs: Vec<&str> = events
            .iter()
            .filter_map(|(event, source)| match (event, source) {
                (AgentEvent::Model(harnx_core::event::ModelEvent::Final { output, .. }), None) => {
                    Some(output.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            final_outputs,
            ["all done"],
            "only the text that ends the full tool loop is final"
        );
    }

    #[tokio::test]
    async fn user_prompt_block_reason_is_emitted_as_error_notice() {
        let _guard = SINK_LOCK.lock().await;
        let sink = Arc::new(CollectingSink::default());
        let reason = "hook server failed to start: Friendly safety guard hook unavailable";

        harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            emit_user_prompt_block_notice(reason.to_string());
        })
        .await;

        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(|(event, source)| {
            source.is_none()
                && matches!(event, AgentEvent::Notice(NoticeEvent::Error(message)) if message == reason)
        }));
    }

    use tempfile::TempDir;

    fn replay_test_config(_tmp: &TempDir) -> GlobalConfig {
        let mut config = Config::default();
        let mut session = crate::config::session::new(&config, "replay_test", None).unwrap();
        crate::config::session::attach_memory_log(&mut session);
        config.session = Some(session);
        Arc::new(ConfigLock::new(config))
    }

    fn usage(input: u64, output: u64, cached: u64, cache_write: u64) -> CompletionTokenUsage {
        CompletionTokenUsage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cache_write_tokens: cache_write,
        }
    }

    fn final_usages(sink: &CollectingSink) -> Vec<CompletionTokenUsage> {
        sink.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(event, source)| match (event, source) {
                (AgentEvent::Model(harnx_core::event::ModelEvent::Final { usage, .. }), None) => {
                    Some(usage.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn no_op_tool_round() -> OnToolRoundFn {
        Arc::new(|_, _| Box::pin(async { Ok(()) }))
    }

    #[tokio::test]
    async fn final_usage_sums_every_completion_in_tool_loop() {
        let _sink_guard = SINK_LOCK.lock().await;
        let _client_guard = crate::client::TestStateGuard::new(None).await;
        let tmp = TempDir::new().unwrap();
        let config = replay_test_config(&tmp);
        let call_count = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&call_count);
        let call_fn: AgentCallFn = Arc::new(move |_, _, _| {
            let count = Arc::clone(&count);
            Box::pin(async move {
                match count.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok((
                        "calling tool".to_owned(),
                        None,
                        vec![ToolCall::new(
                            "noop".to_owned(),
                            json!({}),
                            Some("call-1".to_owned()),
                            None,
                        )],
                        usage(10, 5, 2, 1),
                    )),
                    1 => Ok(("all done".to_owned(), None, vec![], usage(20, 7, 4, 3))),
                    round => panic!("unexpected model round {round}"),
                }
            })
        });
        let ctx = make_test_context(config, call_fn, no_op_tool_round());
        let input = crate::config::input::from_str(&ctx.config, "do work", None);
        let sink = Arc::new(CollectingSink::default());

        let result = harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            run_agent_loop(&ctx, input).await
        })
        .await
        .unwrap();

        assert!(matches!(result, LoopResult::Completed));
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        assert_eq!(final_usages(&sink), [usage(30, 12, 6, 4)]);
    }

    #[tokio::test]
    async fn final_usage_matches_single_non_streaming_completion() {
        let _sink_guard = SINK_LOCK.lock().await;
        let model = priced_metrics_model();
        let mock = Arc::new(
            MockClient::builder()
                .model(model.clone())
                .add_turn(metrics_mock_turn("all done", None, (13, 8, 3, 2)))
                .build(),
        );
        let config = Arc::new(ConfigLock::new(Config {
            data: harnx_core::config_data::ConfigData {
                stream: false,
                ..Default::default()
            },
            model: model.clone(),
            ..Default::default()
        }));
        let mut input = crate::config::input::from_str(&config, "do work", None);
        input.agent_mut().set_model(model);
        let ctx = metrics_loop_context(config);
        let sink = Arc::new(CollectingSink::default());
        let _client_guard = TestStateGuard::new(Some(mock.clone())).await;

        let result = harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            run_agent_loop(&ctx, input).await
        })
        .await
        .unwrap();

        assert!(matches!(result, LoopResult::Completed));
        assert_eq!(mock.conversation_history().conversation_history.len(), 1);
        assert_eq!(final_usages(&sink), [usage(13, 8, 3, 2)]);
    }

    #[tokio::test]
    async fn final_usage_resets_between_turns() {
        let _sink_guard = SINK_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let config = replay_test_config(&tmp);
        let call_count = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&call_count);
        let call_fn: AgentCallFn = Arc::new(move |_, _, _| {
            let count = Arc::clone(&count);
            Box::pin(async move {
                let result = match count.fetch_add(1, Ordering::SeqCst) {
                    0 => ("first turn".to_owned(), None, vec![], usage(11, 3, 2, 1)),
                    1 => ("second turn".to_owned(), None, vec![], usage(7, 5, 1, 0)),
                    round => panic!("unexpected model round {round}"),
                };
                Ok(result)
            })
        });
        let ctx = make_test_context(config, call_fn, no_op_tool_round());
        let first_input = crate::config::input::from_str(&ctx.config, "first", None);
        let second_input = crate::config::input::from_str(&ctx.config, "second", None);
        let sink = Arc::new(CollectingSink::default());

        harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            assert!(matches!(
                run_agent_loop(&ctx, first_input).await.unwrap(),
                LoopResult::Completed
            ));
            assert!(matches!(
                run_agent_loop(&ctx, second_input).await.unwrap(),
                LoopResult::Completed
            ));
        })
        .await;

        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        assert_eq!(final_usages(&sink), [usage(11, 3, 2, 1), usage(7, 5, 1, 0)]);
    }

    /// Regression test for the user-message-replay bug: a user message typed
    /// during a running tool round (delivered via `on_tool_round` setting
    /// `Input::injected_user_text`) must not be re-emitted on every
    /// subsequent loop iteration. Before the fix, `injected_user_text`
    /// stayed set across rounds, so `begin_turn` appended the same user
    /// message N times — once per following round — and the LLM saw N
    /// duplicate copies. The fix clears the field after each round.
    #[tokio::test(flavor = "multi_thread")]
    async fn injected_user_text_is_not_replayed_across_rounds() {
        let _sink_guard = SINK_LOCK.lock().await;
        let _guard = crate::client::TestStateGuard::new(None).await;
        let tmp = TempDir::new().unwrap();
        let global_config = replay_test_config(&tmp);
        // Mock LLM: round 1 → tool call, round 2 → tool call, round 3 → text-only.
        // No real tool provider is registered, so eval_tool_calls returns
        // is_error results. That's fine — the loop still proceeds round by
        // round, which is what the test needs.
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();
        let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
            let cc = cc.clone();
            Box::pin(async move {
                let n = cc.fetch_add(1, Ordering::SeqCst);
                let result = if n < 2 {
                    (
                        format!("calling tool {n}"),
                        None,
                        vec![ToolCall::new(
                            "noop".to_string(),
                            json!({}),
                            Some(format!("call_{n}")),
                            None,
                        )],
                        CompletionTokenUsage::default(),
                    )
                } else {
                    (
                        "all done".to_string(),
                        None,
                        vec![],
                        CompletionTokenUsage::default(),
                    )
                };
                Ok(result)
            })
        });

        // Simulate the TUI's pending-message injection: set
        // `injected_user_text` exactly once, after the first tool round.
        let inj_count = Arc::new(AtomicUsize::new(0));
        let inj = inj_count.clone();
        let on_tool_round: OnToolRoundFn = Arc::new(move |merged_input, _results| {
            let inj = inj.clone();
            Box::pin(async move {
                if inj.fetch_add(1, Ordering::SeqCst) == 0 {
                    merged_input.set_injected_user_text("queued message".to_string());
                }
                Ok(())
            })
        });

        let ctx = make_test_context(global_config.clone(), call_fn, on_tool_round);

        let input = crate::config::input::from_str(&global_config, "do work", None);
        let sink = Arc::new(CollectingSink::default());
        harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            run_agent_loop(&ctx, input).await
        })
        .await
        .unwrap();
        assert_single_completed_turn(&sink);

        // The injection happened once; the bug would have made it appear in
        // round 2 and round 3). With the fix it appears exactly once.
        let cfg = global_config.read();
        let session = cfg.session.as_ref().expect("session attached above");
        let count = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User && m.content.to_text() == "queued message")
            .count();
        assert_eq!(
            count, 1,
            "injected_user_text must be appended once per injection, not \
             replayed on every subsequent loop iteration. Got {count} copies \
             in session.messages."
        );
    }

    fn priced_metrics_model() -> Model {
        let mut model_data = ModelData::new("metrics-model");
        model_data.input_price = Some(3.0);
        model_data.output_price = Some(15.0);
        model_data.cache_read_price = Some(0.3);
        model_data.cache_write_price = Some(3.75);
        Model::from_config("mypkg/openai", &[model_data])
            .into_iter()
            .next()
            .expect("priced model should exist")
    }

    fn metrics_mock_turn(
        text: &str,
        tool_call_id: Option<&str>,
        usage: (u64, u64, u64, u64),
    ) -> MockTurn {
        let (input_tokens, output_tokens, cached_tokens, cache_write_tokens) = usage;
        let tool_calls = tool_call_id
            .map(|id| ToolCall::new("noop".to_owned(), json!({}), Some(id.to_owned()), None))
            .into_iter()
            .collect();
        MockTurnBuilder::new()
            .output(ChatCompletionsOutput {
                text: text.to_owned(),
                tool_calls,
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                cached_tokens: Some(cached_tokens),
                cache_write_tokens: Some(cache_write_tokens),
                ..Default::default()
            })
            .build()
    }

    fn metrics_mock_client(model: &Model) -> Arc<MockClient> {
        Arc::new(
            MockClient::builder()
                .model(model.clone())
                .add_turn(metrics_mock_turn(
                    "first tool",
                    Some("call-1"),
                    (10, 5, 2, 1),
                ))
                .add_turn(metrics_mock_turn(
                    "second tool",
                    Some("call-2"),
                    (20, 7, 4, 3),
                ))
                .add_turn(metrics_mock_turn("all done", None, (30, 11, 8, 5)))
                .build(),
        )
    }

    fn metrics_loop_context(config: GlobalConfig) -> AgentLoopContext {
        AgentLoopContext {
            config,
            instance_id: harnx_core::instance::ServerScope::new(),
            abort_signal: create_abort_signal(),
            token_budget: None,
            usage_at_start: CompletionTokenUsage::default(),
            call_fn: None,
            on_tool_round: None,
            on_text_response: None,
            initial_with_embeddings: false,
            initial_resume_count: 0,
            max_resume: Some(0),
            prepare_handoff_fn: None,

            pending_async_context: None,
            surface_notes: None,
            working_dir: None,
            tool_loop_guard: Default::default(),
        }
    }

    fn run_metrics_tool_loop(
        model: &Model,
        mock: Arc<MockClient>,
        clients: Vec<ClientConfig>,
    ) -> Vec<DebugMetric> {
        let global_config = Arc::new(ConfigLock::new(Config {
            data: harnx_core::config_data::ConfigData {
                stream: false,
                ..Default::default()
            },
            model: model.clone(),
            clients,
            ..Default::default()
        }));
        let mut input = crate::config::input::from_str(&global_config, "do work", None);
        input.agent_mut().set_name("metrics-agent");
        input.agent_mut().set_model(model.clone());
        let ctx = metrics_loop_context(global_config);
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime should build")
                .block_on(async {
                    let _guard = TestStateGuard::new(Some(mock.clone())).await;
                    let result = run_agent_loop(&ctx, input)
                        .await
                        .expect("tool loop should complete");
                    assert!(matches!(result, LoopResult::Completed));
                });
        });
        snapshotter.snapshot().into_vec()
    }

    #[test]
    fn three_call_tool_loop_records_llm_metrics_once_per_call() {
        harnx_core::require_nextest();
        let model = priced_metrics_model();
        let mock = metrics_mock_client(&model);
        let snapshot = run_metrics_tool_loop(&model, mock.clone(), vec![]);
        assert_eq!(
            mock.conversation_history().conversation_history.len(),
            3,
            "mock should receive three model calls"
        );

        assert_eq!(
            token_metric_value(&snapshot, "input"),
            &DebugValue::Counter(60),
            "10 + 20 + 30 must be recorded once; event duplication would produce 120 and final duplication would produce 90"
        );
        assert_eq!(
            token_metric_value(&snapshot, "output"),
            &DebugValue::Counter(23)
        );
        assert_eq!(
            token_metric_value(&snapshot, "cached"),
            &DebugValue::Counter(14)
        );
        assert_eq!(
            token_metric_value(&snapshot, "cache_read"),
            &DebugValue::Counter(14)
        );
        assert_eq!(
            token_metric_value(&snapshot, "cache_write"),
            &DebugValue::Counter(9)
        );

        let cost_value = metric_value(
            &snapshot,
            MetricKind::Gauge,
            harnx_metrics::LLM_COST_DOLLARS,
            &[
                ("agent", "metrics-agent"),
                ("client", "mypkg/openai"),
                ("model", "metrics-model"),
                ("provider", ""),
            ],
        );
        let DebugValue::Gauge(actual_cost) = cost_value else {
            panic!("cost should be a floating-point cumulative gauge, got {cost_value:?}");
        };
        let expected_usage = CompletionTokenUsage {
            input_tokens: 60,
            output_tokens: 23,
            cached_tokens: 14,
            cache_write_tokens: 9,
        };
        let expected_cost = model
            .cost_usd(&expected_usage)
            .expect("test model has input, cache-read, cache-write, and output prices");
        assert!(
            (actual_cost.into_inner() - expected_cost).abs() < 1e-12,
            "cost should equal sum of three calls"
        );
    }

    #[test]
    fn unpriced_calls_with_tokens_are_counted_not_silently_dropped() {
        harnx_core::require_nextest();
        // #2638's visibility gap: a model with input/output prices but no
        // cache-read price returns None for every cached-token call, and
        // those calls used to leave no cost-metric trace at all.
        let mut model_data = ModelData::new("metrics-model");
        model_data.input_price = Some(3.0);
        model_data.output_price = Some(15.0);
        let model = Model::from_config("mypkg/openai", &[model_data])
            .into_iter()
            .next()
            .expect("model should exist");
        let mock = metrics_mock_client(&model);
        let snapshot = run_metrics_tool_loop(&model, mock, vec![]);

        assert_eq!(
            token_metric_value(&snapshot, "cached"),
            &DebugValue::Counter(14),
            "tokens are still recorded for unpriced calls"
        );

        // Three calls, each with cached tokens the model cannot price: every
        // one must be counted, and the cost gauge must stay absent.
        assert_eq!(
            metric_value(
                &snapshot,
                MetricKind::Counter,
                harnx_metrics::LLM_UNPRICED_CALLS_TOTAL,
                &[
                    ("agent", "metrics-agent"),
                    ("client", "mypkg/openai"),
                    ("model", "metrics-model"),
                    ("provider", ""),
                ],
            ),
            &DebugValue::Counter(3)
        );
        let cost_key = metric_key(
            MetricKind::Gauge,
            harnx_metrics::LLM_COST_DOLLARS,
            &[
                ("agent", "metrics-agent"),
                ("client", "mypkg/openai"),
                ("model", "metrics-model"),
                ("provider", ""),
            ],
        );
        assert!(
            !snapshot.iter().any(|(key, ..)| key == &cost_key),
            "unpriced calls must not increment the cost gauge"
        );
    }

    #[test]
    fn configured_client_records_canonical_provider_label() {
        harnx_core::require_nextest();
        let model = priced_metrics_model();
        let mock = metrics_mock_client(&model);
        let mut client = ClientConfig::OpenAIConfig(Default::default());
        client.set_name("mypkg/openai".to_owned());

        let snapshot = run_metrics_tool_loop(&model, mock, vec![client]);

        for (usage_type, expected_total) in [
            ("input", 60),
            ("output", 23),
            ("cached", 14),
            ("cache_read", 14),
            ("cache_write", 9),
        ] {
            assert_eq!(
                metric_value(
                    &snapshot,
                    MetricKind::Counter,
                    harnx_metrics::LLM_TOKENS_TOTAL,
                    &[
                        ("agent", "metrics-agent"),
                        ("client", "mypkg/openai"),
                        ("model", "metrics-model"),
                        ("provider", "openai"),
                        ("type", usage_type),
                    ],
                ),
                &DebugValue::Counter(expected_total),
            );
        }
    }

    fn handoff_on_tool_round() -> OnToolRoundFn {
        Arc::new(move |_merged_input, results| {
            Box::pin(async move {
                let result = results
                    .first()
                    .expect("handoff test should produce single tool result");
                assert_eq!(result.call.name, "delegate-agent_session_handoff");
                Ok(())
            })
        })
    }

    fn make_test_context(
        global_config: GlobalConfig,
        call_fn: AgentCallFn,
        on_tool_round: OnToolRoundFn,
    ) -> AgentLoopContext {
        AgentLoopContext {
            instance_id: harnx_core::instance::ServerScope::new(),
            config: global_config,
            abort_signal: create_abort_signal(),
            token_budget: None,
            usage_at_start: CompletionTokenUsage::default(),
            call_fn: Some(call_fn),
            on_tool_round: Some(on_tool_round),
            on_text_response: None,
            working_dir: None,
            initial_with_embeddings: false,
            initial_resume_count: 0,
            max_resume: Some(0),
            prepare_handoff_fn: None,

            pending_async_context: None,
            surface_notes: None,
            tool_loop_guard: Default::default(),
        }
    }

    #[tokio::test]
    async fn cancelled_turn_emits_neither_normal_end_nor_model_error() {
        let _guard = SINK_LOCK.lock().await;
        let config = Arc::new(ConfigLock::new(crate::config::Config::default()));
        let call_fn: AgentCallFn = Arc::new(|_, _, _| unreachable!("model is not called"));
        let ctx = make_test_context(config, call_fn, handoff_on_tool_round());
        ctx.abort_signal.set_ctrlc();
        let sink = Arc::new(CollectingSink::default());

        let result: Result<()> = harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            with_turn_lifecycle(&ctx, async { Err(anyhow::anyhow!("interrupted by user")) }).await
        })
        .await;
        assert!(result.is_err());

        let events = sink.events.lock().unwrap();
        assert!(matches!(
            events.as_slice(),
            [(AgentEvent::Turn(TurnEvent::Started), None)]
        ));
    }

    #[tokio::test]
    async fn shared_pending_context_is_taken_and_injected_into_next_input() {
        let config = Arc::new(ConfigLock::new(crate::config::Config::default()));
        let mut input = crate::config::input::from_str(&config, "user prompt", None);
        let pending = Arc::new(tokio::sync::Mutex::new(Some(
            "context queued over NATS".to_string(),
        )));

        inject_shared_pending_context(&mut input, Some(&pending)).await;

        // Hook context rides `hook_context` (issue #2198): prepending it to
        // the prompt text used to be dropped mid-turn and on worker turns.
        assert_eq!(input.text(), "user prompt");
        assert_eq!(input.hook_context(), Some("context queued over NATS"));
        assert!(pending.lock().await.is_none());
    }

    fn handoff_event_matches_expected((event, source): &(AgentEvent, Option<AgentSource>)) -> bool {
        matches!(
            event,
            AgentEvent::Turn(TurnEvent::HandoffRequested { agent, session_id })
                if agent == "delegate-agent"
                    && session_id.as_deref() == Some("handoff-target-session")
        ) && source.is_none()
    }

    /// A synthesized handoff result must terminate the loop segment and emit
    /// an unwrapped source-stream request before dispatch begins.
    #[tokio::test]
    async fn handoff_result_returns_request_and_emits_event() {
        let _guard = SINK_LOCK.lock().await;
        harnx_core::sink::clear_agent_event_sink();

        let config = Arc::new(ConfigLock::new(crate::config::Config::default()));
        let call_fn: AgentCallFn = Arc::new(|_, _, _| unreachable!("model is not called"));
        let ctx = make_test_context(config.clone(), call_fn, handoff_on_tool_round());
        let input = crate::config::input::from_str(&config, "start handoff", None);
        let call = ToolCall::new(
            "delegate-agent_session_handoff".to_string(),
            json!({ "prompt": "finish delegated work" }),
            Some("handoff-call-1".to_string()),
            None,
        );
        let mut tool_result = harnx_core::tool::ToolResult::new(call, json!({}));
        tool_result.switch_agent = Some(harnx_core::tool::SwitchAgentData {
            agent: "delegate-agent".to_string(),
            prompt: "finish delegated work".to_string(),
            session_id: Some("handoff-target-session".to_string()),
            tool_call_id: None,
        });

        let sink = Arc::new(CollectingSink::default());
        let result = harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            advance_tool_round(
                &ctx,
                input,
                ToolRoundOutput {
                    output: "handoff now".to_string(),
                    thought: None,
                    tool_results: vec![tool_result],
                },
            )
            .await
        })
        .await
        .unwrap();

        let ToolRoundAdvance::Handoff(LoopResult::HandoffRequested {
            agent,
            session_id,
            prompt,
            tool_call_id: _,
        }) = result
        else {
            panic!("expected handoff advance");
        };
        assert_eq!(agent, "delegate-agent");
        assert_eq!(session_id.as_deref(), Some("handoff-target-session"));
        assert_eq!(prompt, "finish delegated work");
        assert!(sink
            .events
            .lock()
            .unwrap()
            .iter()
            .any(handoff_event_matches_expected));

        harnx_core::sink::clear_agent_event_sink();
    }
}
