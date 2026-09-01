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
    nats_hook_provider::{dispatch_hook_event, HookDispatchMeta, HookEventDispatch},
    tool::{execute_tool_round, CompletionText, ToolResult},
    utils::dimmed_text,
};
use anyhow::{bail, Result};
use harnx_hooks::{inject_pending_async_context, HookEvent, HookResultControl};
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
    pub nats_hook_provider: Option<Arc<crate::nats_hook_provider::NatsHookProvider>>,
    pub pending_async_context: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    /// Optional per-session working directory. When unset, runtime falls back
    /// to process cwd for CLI compatibility.
    pub working_dir: Option<PathBuf>,
}

/// Resume a tool round that was interrupted for approval.
///
/// This is the continuation seam for Design B interrupt/resume:
/// - Takes saved assistant output/thought from the interrupted round
/// - Takes the pending ToolCalls that were deferred
/// - Takes resolved approve/deny decisions for each call
/// - Executes the approved calls with a preseeded confirm function
/// - Persists ToolResults via existing session helpers
/// - Continues the canonical loop from the post-tool state
pub async fn continue_agent_loop_from_tool_round(
    ctx: &AgentLoopContext,
    mut input: Input,
    output: String,
    thought: Option<String>,
    tool_calls: Vec<ToolCall>,
    decisions: Vec<ToolApprovalDecision>,
    pending_interrupt_ids: std::collections::BTreeSet<String>,
) -> Result<LoopResult> {
    use crate::tool::ToolApprovalInterrupt;

    let config = &ctx.config;

    // Build a preseeded confirm function that returns decisions for known call IDs.
    let decisions = std::sync::Arc::new(decisions);
    let decisions_for_confirm = std::sync::Arc::clone(&decisions);
    let confirm_override: std::sync::Arc<crate::tool::ConfirmToolUseFn> = std::sync::Arc::new(
        move |call: &ToolCall, _args: &serde_json::Value, reason: Option<&str>| {
            if let Some(call_id) = call.id.as_deref() {
                if let Some(decision) = decisions_for_confirm
                    .iter()
                    .find(|d| d.tool_call_id == call_id)
                {
                    return if decision.approved {
                        crate::tool::ToolUseConfirmation::Approve
                    } else {
                        crate::tool::ToolUseConfirmation::Deny {
                            reason: decision
                                .reason
                                .clone()
                                .or_else(|| reason.map(str::to_string)),
                        }
                    };
                }
                if !pending_interrupt_ids.contains(call_id) {
                    return crate::tool::ToolUseConfirmation::Approve;
                }
            }
            crate::tool::ToolUseConfirmation::Defer
        },
    );

    // Install the override for this resumption
    config
        .write()
        .set_tui_confirm_tool_use(Some(confirm_override));

    // Execute full pending tool round using normal helper. Deferred calls resolve via
    // preseeded confirm function; already-approved calls execute normally.
    let tool_results = match crate::tool::execute_tool_round_with_persistence(
        ctx.tool_round_params(
            config,
            &input,
            CompletionText {
                output: &output,
                thought: thought.as_deref(),
            },
        ),
        tool_calls,
        crate::tool::ToolRoundPersistence::REUSE_EXISTING_CALLS,
    )
    .await
    {
        Ok(results) => results,
        Err(err) => {
            // If we hit another interrupt, propagate it (shouldn't happen with preseeded decisions)
            if ToolApprovalInterrupt::from_error(&err).is_some() {
                return Err(err);
            }
            // Other errors: propagate
            return Err(err);
        }
    };

    // Merge tool results into input for the next round
    if !tool_results.is_empty() {
        let switch_agent = tool_results.iter().find_map(|v| v.switch_agent.clone());
        let mut merged_input =
            input.merge_tool_results(output.clone(), thought.clone(), tool_results.clone());

        // Invoke on_tool_round callback
        if let Some(ref cb) = ctx.on_tool_round {
            cb(&mut merged_input, &tool_results).await?;
        }

        if switch_agent.is_some() {
            return run_agent_loop(ctx, merged_input).await;
        }

        input = merged_input;
    }

    // Continue the canonical loop from post-tool state
    run_agent_loop(ctx, input).await
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
    HandoffRequested {
        agent: String,
        session_id: Option<String>,
        prompt: String,
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
        return run_agent_loop_inner(ctx, initial_input).await;
    }

    with_turn_lifecycle(ctx, run_agent_loop_inner(ctx, initial_input)).await
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
        let result = run_agent_loop_inner(ctx, initial_input).await?;
        before_end(&result).await?;
        return Ok(result);
    }

    emit_turn_started();
    let result = async {
        let result = run_agent_loop_inner(ctx, initial_input).await?;
        before_end(&result).await?;
        Ok(result)
    }
    .await;
    emit_turn_ended(ctx, &result);
    result
}

async fn with_turn_lifecycle<T>(
    ctx: &AgentLoopContext,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    emit_turn_started();
    let result = future.await;
    emit_turn_ended(ctx, &result);
    result
}

fn emit_turn_started() {
    use harnx_core::event::{AgentEvent, TurnEvent};

    harnx_core::sink::emit_agent_event(AgentEvent::Turn(TurnEvent::Started));
}

fn emit_turn_ended<T>(ctx: &AgentLoopContext, result: &Result<T>) {
    use harnx_core::event::{AgentEvent, ModelEvent, TurnEvent, TurnOutcome};

    if let Err(error) = result {
        if !ctx.abort_signal.aborted() {
            harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::Error(
                harnx_render::pretty_error_string(error),
            )));
        }
    }
    harnx_core::sink::emit_agent_event(AgentEvent::Turn(TurnEvent::Ended {
        outcome: TurnOutcome::default(),
    }));
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
        return run_agent_loop_inner(ctx, input).await.map(|_| ());
    }

    with_turn_lifecycle(ctx, async move {
        loop {
            match run_agent_loop_inner(ctx, input).await? {
                LoopResult::Completed => return Ok(()),
                LoopResult::HandoffRequested {
                    agent,
                    session_id,
                    prompt,
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

struct AgentHookDispatch<'a> {
    ctx: &'a AgentLoopContext,
    event: HookEvent,
    session_id: &'a str,
    cwd: &'a std::path::Path,
    resume_count: u32,
}

async fn dispatch_agent_loop_hook(params: AgentHookDispatch<'_>) -> harnx_core::hooks::HookOutcome {
    let AgentHookDispatch {
        ctx,
        event,
        session_id,
        cwd,
        resume_count,
    } = params;
    dispatch_hook_event(HookEventDispatch {
        event,
        provider: ctx.nats_hook_provider.as_deref(),
        meta: HookDispatchMeta {
            session_id: session_id.to_string(),
            cwd: cwd.to_path_buf(),
            resume_count,
        },
        pending_async_context: ctx.pending_async_context.clone(),
    })
    .await
}

async fn inject_shared_pending_context(
    input: &mut Input,
    shared_pending: Option<&Arc<tokio::sync::Mutex<Option<String>>>>,
) {
    if let Some(shared_pending) = shared_pending {
        let mut pending_guard = shared_pending.lock().await;
        let mut pending = pending_guard.take();
        inject_pending_async_context(input, &mut pending);
        *pending_guard = pending;
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
    while config.read().is_compacting_session() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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

async fn user_prompt_block_reason(
    ctx: &AgentLoopContext,
    input: &Input,
    turn: &TurnHookContext,
    resume_count: u32,
) -> Option<String> {
    let outcome = dispatch_agent_loop_hook(AgentHookDispatch {
        ctx,
        event: HookEvent::UserPromptSubmit {
            prompt: input.text().to_string(),
        },
        session_id: &turn.session_id,
        cwd: &turn.cwd,
        resume_count,
    })
    .await;
    match outcome.control {
        HookResultControl::Block { reason } => Some(reason),
        _ => None,
    }
}

fn emit_user_prompt_block_notice(reason: String) {
    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Notice(
        harnx_core::event::NoticeEvent::Error(reason),
    ));
}

async fn pre_model_call_boundary_passes(
    ctx: &AgentLoopContext,
    input: &Input,
    turn: &TurnHookContext,
    resume_count: u32,
) -> Result<bool> {
    if let Some(reason) = user_prompt_block_reason(ctx, input, turn, resume_count).await {
        emit_user_prompt_block_notice(reason);
        return Ok(false);
    }

    enforce_token_budget(ctx)?;
    Ok(true)
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
    let _ = dispatch_agent_loop_hook(AgentHookDispatch {
        ctx,
        event: HookEvent::StopFailure {
            error: error.to_string(),
            error_type: "api_error".to_string(),
        },
        session_id: &turn.session_id,
        cwd: &turn.cwd,
        resume_count,
    })
    .await;
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

async fn dispatch_text_stop(
    params: TextStopDispatch<'_>,
) -> Option<harnx_core::hooks::HookOutcome> {
    if params.has_tool_results {
        return None;
    }
    let outcome = dispatch_agent_loop_hook(AgentHookDispatch {
        ctx: params.ctx,
        event: HookEvent::Stop {
            stop_hook_active: true,
            last_assistant_message: Some(params.output.to_string()),
        },
        session_id: &params.turn.session_id,
        cwd: &params.turn.cwd,
        resume_count: params.resume_count,
    })
    .await;
    if let Some(context) = outcome
        .result
        .additional_context
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        debug!("Captured Stop hook additional context for later auto-continue: {context}");
    }
    Some(outcome)
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
    let switch_agent = round
        .tool_results
        .iter()
        .find_map(|result| result.switch_agent.clone());
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
        }));
    }
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
    outcome: Option<harnx_core::hooks::HookOutcome>,
) -> ResumeAction {
    let Some(outcome) = outcome else {
        return ResumeAction::None;
    };
    if !outcome.result.resume.unwrap_or(false) || resume_count >= turn.max_resume {
        return ResumeAction::None;
    }
    if ctx.abort_signal.aborted() {
        return ResumeAction::Abort;
    }
    ResumeAction::Context(
        outcome
            .result
            .additional_context
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Continue working on pending tasks.".to_string()),
    )
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
        return Err(anyhow::Error::msg(crate::budget_terminal_message(
            budgeted, budget,
        )));
    }
    Ok(())
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
async fn run_agent_loop_inner(ctx: &AgentLoopContext, initial_input: Input) -> Result<LoopResult> {
    record_agent_turn_attributes(ctx);

    let config = &ctx.config;
    let abort_signal = &ctx.abort_signal;
    let mut input = initial_input;
    let mut resume_count: u32 = ctx.initial_resume_count;
    let mut with_embeddings = ctx.initial_with_embeddings;
    let mut emitted_text_turns: u32 = 0;

    loop {
        if input.is_empty() {
            break;
        }

        wait_for_session_compaction(config).await;
        apply_round_embeddings(&mut input, config, abort_signal, with_embeddings).await?;

        // Inject context queued directly by the NATS hook provider.
        inject_shared_pending_context(&mut input, ctx.pending_async_context.as_ref()).await;

        config.write().before_chat_completion(&input)?;

        let turn = turn_hook_context(ctx);
        if !pre_model_call_boundary_passes(ctx, &input, &turn, resume_count).await? {
            break;
        }

        let llm_result = call_agent_model(ctx, &mut input).await;

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

        let tool_results = complete_model_turn(
            ctx,
            &input,
            CompletionOutput {
                output: &output,
                thought: thought.as_deref(),
                tool_calls,
                usage: &usage,
            },
        )
        .await?;

        // `injected_user_text` is a one-shot field — it was written to the
        // session by `begin_turn` (inside `add_assistant_text` /
        // `add_tool_calls`) just above. Clear it now so it isn't re-emitted
        // on every subsequent loop iteration; on_tool_round may set a fresh
        // injection from the next pending user message below.
        input.injected_user_text = None;

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

        emit_final_text_response(ctx, output, usage).await;

        // Done.
        break;
    }

    finish_agent_loop(config, abort_signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ChatCompletionsOutput, MessageRole, Model, ModelData, TestStateGuard};
    use crate::test_utils::{MockClient, MockTurn, MockTurnBuilder};
    use crate::utils::create_abort_signal;
    use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, NoticeEvent, TurnEvent};
    use metrics::{Key, Label};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey, MetricKind,
    };
    use parking_lot::RwLock;
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
            Some((AgentEvent::Turn(TurnEvent::Ended { .. }), None))
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
        Arc::new(RwLock::new(config))
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
            nats_hook_provider: None,
            pending_async_context: None,
            working_dir: None,
        }
    }

    fn run_metrics_tool_loop(model: &Model, mock: Arc<MockClient>) -> Vec<DebugMetric> {
        let global_config = Arc::new(RwLock::new(Config {
            data: harnx_core::config_data::ConfigData {
                stream: false,
                ..Default::default()
            },
            model: model.clone(),
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
        let snapshot = run_metrics_tool_loop(&model, mock.clone());
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
            nats_hook_provider: None,
            pending_async_context: None,
        }
    }

    #[tokio::test]
    async fn cancelled_turn_ends_without_emitting_model_error() {
        let _guard = SINK_LOCK.lock().await;
        let config = Arc::new(RwLock::new(crate::config::Config::default()));
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
            [
                (AgentEvent::Turn(TurnEvent::Started), None),
                (AgentEvent::Turn(TurnEvent::Ended { .. }), None)
            ]
        ));
    }

    #[tokio::test]
    async fn shared_pending_context_is_taken_and_injected_into_next_input() {
        let config = Arc::new(RwLock::new(crate::config::Config::default()));
        let mut input = crate::config::input::from_str(&config, "user prompt", None);
        let pending = Arc::new(tokio::sync::Mutex::new(Some(
            "context queued over NATS".to_string(),
        )));

        inject_shared_pending_context(&mut input, Some(&pending)).await;

        assert_eq!(
            input.text(),
            "context queued over NATS

user prompt"
        );
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

        let config = Arc::new(RwLock::new(crate::config::Config::default()));
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
