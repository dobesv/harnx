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
    config::{session_lock::SessionLock, Config, GlobalConfig, Input},
    nats_hook_provider::{dispatch_hook_event, HookDispatchMeta, HookEventDispatch},
    tool::{execute_tool_round, CompletionText, ToolResult},
    utils::dimmed_text,
};
use anyhow::{bail, Context, Result};
use harnx_hooks::{
    dispatch_hooks_with_count_and_manager, drain_async_results, inject_pending_async_context,
    AsyncHookManager, HookEvent, HookResultControl, PersistentHookManager,
};
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
    dyn for<'a> Fn(&'a mut Input, &'a [ToolResult]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
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
    pub instance_id: harnx_core::instance::InstanceId,
    pub abort_signal: AbortSignal,
    /// Async hook manager (shared, mutex-protected). The CLI wraps its
    /// `&mut AsyncHookManager` into an `Arc<Mutex<...>>` for the duration of
    /// the call.
    pub async_manager: Arc<tokio::sync::Mutex<AsyncHookManager>>,
    /// Persistent hook manager (shared, mutex-protected).
    pub persistent_manager: Arc<tokio::sync::Mutex<PersistentHookManager>>,
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
    /// Optional session lock held across the agent loop (T6).
    /// When set, passed through to exit paths to avoid re-entrancy deadlock.
    #[allow(dead_code)]
    pub session_lock: Option<SessionLock>,
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
            cb(&mut merged_input, &tool_results).await;
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
    let session_path = {
        let config_read = ctx.config.read();
        match config_read.session.as_ref() {
            Some(session) if session.save_session() != Some(false) => {
                session.path.as_deref().map(PathBuf::from).or_else(|| {
                    session
                        .sessions_dir
                        .as_ref()
                        .map(|sessions_dir| sessions_dir.join(format!("{}.yaml", session.id)))
                })
            }
            _ => None,
        }
    };

    // Acquire session lock if needed, or use the one from ctx
    let _session_lock: Option<SessionLock> = if let Some(session_path) = session_path {
        // Use the lock from ctx if available (preferred)
        if let Some(ref _lock) = ctx.session_lock {
            // We don't clone SessionLock (File isn't Clone), but we don't need to -
            // the lock in ctx keeps it held. We just pass a reference through.
            None
        } else {
            // Otherwise acquire it ourselves
            let lock = match SessionLock::try_acquire(&session_path)? {
                Some(lock) => lock,
                None => {
                    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Notice(
                        harnx_core::event::NoticeEvent::Info(
                            "Waiting for session lock…".to_string(),
                        ),
                    ));
                    let sp = session_path.clone();
                    tokio::task::spawn_blocking(move || SessionLock::acquire(&sp))
                        .await
                        .context("session lock task join failed")??
                }
            };
            crate::config::reload_session_from_disk(&ctx.config)?;
            Some(lock)
        }
    } else {
        None
    };

    run_agent_loop_inner(
        ctx,
        initial_input,
        ctx.session_lock.as_ref().or(_session_lock.as_ref()),
    )
    .await
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
    loop {
        match run_agent_loop(ctx, input).await? {
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
}

async fn apply_local_handoff(
    ctx: &AgentLoopContext,
    agent: &str,
    session_id: Option<&str>,
    _prompt: &str,
) -> Result<()> {
    ctx.config
        .write()
        .exit_agent_with_lock(ctx.session_lock.as_ref())?;
    Config::use_agent(&ctx.config, agent, session_id, ctx.abort_signal.clone()).await?;
    if ctx.config.read().session.is_some() {
        ctx.config
            .write()
            .empty_session_with_lock(ctx.session_lock.as_ref())?;
    }
    Ok(())
}

struct AgentHookDispatch<'a> {
    ctx: &'a AgentLoopContext,
    event: HookEvent,
    hooks: &'a harnx_hooks::HooksConfig,
    session_id: &'a str,
    cwd: &'a std::path::Path,
    resume_count: u32,
}

async fn dispatch_agent_loop_hook(params: AgentHookDispatch<'_>) -> harnx_core::hooks::HookOutcome {
    let AgentHookDispatch {
        ctx,
        event,
        hooks,
        session_id,
        cwd,
        resume_count,
    } = params;
    let async_guard = ctx.async_manager.lock().await;
    let inline_event = event.clone();
    let inline_fallback = dispatch_hooks_with_count_and_manager(
        &inline_event,
        &hooks.entries,
        session_id,
        cwd,
        resume_count,
        Some(&async_guard),
        Some(&ctx.persistent_manager),
    );
    dispatch_hook_event(
        HookEventDispatch {
            event,
            provider: ctx.nats_hook_provider.as_deref(),
            meta: HookDispatchMeta {
                session_id: session_id.to_string(),
                cwd: cwd.to_path_buf(),
                resume_count,
            },
            pending_async_context: ctx.pending_async_context.clone(),
        },
        inline_fallback,
    )
    .await
}

async fn run_agent_loop_inner(
    ctx: &AgentLoopContext,
    initial_input: Input,
    _session_lock: Option<&SessionLock>,
) -> Result<LoopResult> {
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

        // Wait for any ongoing session compaction to finish.
        while config.read().is_compacting_session() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Apply embeddings on the first round and after agent switches.
        if with_embeddings {
            crate::config::input::use_embeddings(&mut input, config, abort_signal.clone()).await?;
        }

        // Drain completed async hooks and inject any pending context.
        {
            let mut async_guard = ctx.async_manager.lock().await;
            let mut pending: Option<String> = None;
            if let Some(shared_pending) = &ctx.pending_async_context {
                let mut pending_guard = shared_pending.lock().await;
                pending = pending_guard.take();
            }
            drain_async_results(&mut async_guard, &mut pending);
            inject_pending_async_context(&mut input, &mut pending);
            if let Some(shared_pending) = &ctx.pending_async_context {
                let mut pending_guard = shared_pending.lock().await;
                *pending_guard = pending;
            }
        }

        config.write().before_chat_completion(&input)?;

        let (hooks, session_id, cwd) = {
            let cfg = config.read();
            (
                cfg.resolved_hooks(),
                cfg.session
                    .as_ref()
                    .map(|s| s.id().to_string())
                    .unwrap_or_else(|| "default".to_string()),
                ctx.working_dir
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
            )
        };

        let max_resume = ctx
            .max_resume
            .unwrap_or_else(|| hooks.max_resume.unwrap_or(5));

        // Dispatch UserPromptSubmit hook (was previously TUI-only; now unified).
        {
            let event = HookEvent::UserPromptSubmit {
                prompt: input.text().to_string(),
            };
            let outcome = dispatch_agent_loop_hook(AgentHookDispatch {
                ctx,
                event,
                hooks: &hooks,
                session_id: &session_id,
                cwd: &cwd,
                resume_count,
            })
            .await;
            if matches!(outcome.control, HookResultControl::Block { .. }) {
                break;
            }
        }

        // LLM call (with retry + fallback).
        let llm_result = if let Some(ref call_fn) = ctx.call_fn {
            call_fn(&mut input, config, abort_signal.clone()).await
        } else {
            // Use the default call function, which respects config.stream:
            // streaming (call_chat_completions_streaming) when enabled, or
            // non-streaming (call_chat_completions) otherwise. Critically,
            // call_chat_completions_streaming does NOT write to stdout, which
            // matters for server mode where stdout is a protocol
            // transport. The old hardcoded call_chat_completions(inp, true, ...)
            // always printed to stdout, corrupting the connection.
            call_with_retry_and_fallback(&mut input, config, abort_signal.clone()).await
        };

        let (output, thought, tool_calls, usage) = match llm_result {
            Ok(result) => result,
            Err(err) => {
                // LLM error — dispatch StopFailure hook and persist empty turn.
                let event = HookEvent::StopFailure {
                    error: err.to_string(),
                    error_type: "api_error".to_string(),
                };
                let _ = dispatch_agent_loop_hook(AgentHookDispatch {
                    ctx,
                    event,
                    hooks: &hooks,
                    session_id: &session_id,
                    cwd: &cwd,
                    resume_count,
                })
                .await;
                let _ = config.write().after_chat_completion(
                    &input,
                    "",
                    None,
                    &[],
                    &Default::default(),
                );
                return Err(err);
            }
        };

        // Persist + run tools (if any), or persist plain text response.
        let tool_results = if tool_calls.is_empty() {
            config.write().after_chat_completion(
                &input,
                &output,
                thought.as_deref(),
                &[],
                &usage,
            )?;
            Vec::new()
        } else {
            config.write().record_completion_usage(&usage);
            execute_tool_round(
                ctx.tool_round_params(
                    config,
                    &input,
                    CompletionText {
                        output: &output,
                        thought: thought.as_deref(),
                    },
                ),
                tool_calls,
            )
            .await?
        };

        // `injected_user_text` is a one-shot field — it was written to the
        // session by `begin_turn` (inside `add_assistant_text` /
        // `add_tool_calls`) just above. Clear it now so it isn't re-emitted
        // on every subsequent loop iteration; on_tool_round may set a fresh
        // injection from the next pending user message below.
        input.injected_user_text = None;

        // Emit status/usage line for text-only turns. CLI-only: fires when
        // no on_text_response callback is set. TUI and server frontends handle their own
        // display via on_text_response or their own UI.
        if ctx.on_text_response.is_none() && tool_results.is_empty() {
            let config_read = config.read();
            let macro_flag = config_read.macro_flag;
            let status = config_read.render_status_line(true);
            let session_usage = config_read
                .session
                .as_ref()
                .map(|s| s.completion_usage().clone());
            let display_usage = session_usage.as_ref().unwrap_or(&usage);
            let context_stats = config_read
                .session
                .as_ref()
                .map(|s| {
                    let (tokens, percent) = s.tokens_usage();
                    if percent > 0.0 {
                        format!("💬 {}({:.0}%)", tokens, percent)
                    } else {
                        format!("💬 {}", tokens)
                    }
                })
                .unwrap_or_default();
            drop(config_read);
            let mut line_parts = vec![];
            if !status.is_empty() {
                line_parts.push(status);
            }
            if !display_usage.is_empty() {
                line_parts.push(format!("   {}", display_usage));
            }
            if !context_stats.is_empty() {
                line_parts.push(format!("  {}", context_stats));
            }
            if !line_parts.is_empty() {
                let prefix = if macro_flag || emitted_text_turns == 0 {
                    ""
                } else {
                    "\n"
                };
                crate::utils::emit_info(format!("{prefix}{}", dimmed_text(&line_parts.join(""))));
            }
        }

        // Dispatch Stop hook for pure-text turns (no tools).
        let stop_outcome = if tool_results.is_empty() {
            let event = HookEvent::Stop {
                stop_hook_active: true,
                last_assistant_message: Some(output.clone()),
            };
            let outcome = dispatch_agent_loop_hook(AgentHookDispatch {
                ctx,
                event,
                hooks: &hooks,
                session_id: &session_id,
                cwd: &cwd,
                resume_count,
            })
            .await;
            if let Some(additional_context) = outcome
                .result
                .additional_context
                .as_deref()
                .filter(|v| !v.is_empty())
            {
                debug!(
                    "Captured Stop hook additional context for later auto-continue: \
                     {additional_context}"
                );
            }
            Some(outcome)
        } else {
            None
        };

        if !tool_results.is_empty() {
            // Check for agent switch request.
            let switch_agent = tool_results.iter().find_map(|v| v.switch_agent.clone());

            // Merge tool results into input for the next round.
            let mut merged_input = input.merge_tool_results(output, thought, tool_results.clone());

            // Invoke the on_tool_round callback (TUI uses this for
            // ToolRoundComplete + pending message injection).
            if let Some(ref cb) = ctx.on_tool_round {
                cb(&mut merged_input, &tool_results).await;
            }

            if let Some(switch) = switch_agent {
                let source = harnx_core::event::AgentSource {
                    agent: switch.agent.clone(),
                    session_id: switch.session_id.clone(),
                    model: ctx.config.read().current_model_id(),
                };
                harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::sub_agent(
                    source,
                    harnx_core::event::AgentEvent::Turn(
                        harnx_core::event::TurnEvent::HandoffRequested {
                            agent: switch.agent.clone(),
                            session_id: switch.session_id.clone(),
                        },
                    ),
                ));
                return Ok(LoopResult::HandoffRequested {
                    agent: switch.agent.clone(),
                    session_id: switch.session_id.clone(),
                    prompt: switch.prompt.clone(),
                });
            }

            // Normal tool round: loop with merged input.
            input = merged_input;
            with_embeddings = false;
            continue;
        }

        // Text-only turn — invoke on_text_response callback (TUI emits Final).
        if let Some(ref cb) = ctx.on_text_response {
            cb(output.clone(), usage.clone()).await;
        }
        emitted_text_turns += 1;

        // Check if stop hook wants to auto-resume.
        if let Some(outcome) = stop_outcome {
            if outcome.result.resume.unwrap_or(false) && resume_count < max_resume {
                if abort_signal.aborted() {
                    break;
                }
                let context = outcome
                    .result
                    .additional_context
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "Continue working on pending tasks.".to_string());
                input = crate::config::input::from_str(config, &context, None);
                resume_count += 1;
                with_embeddings = true;
                continue;
            }
        }

        // Check async hook results for auto-continue.
        let async_resume_context = {
            let mut async_guard = ctx.async_manager.lock().await;
            let mut pending: Option<String> = None;
            if drain_async_results(&mut async_guard, &mut pending) && resume_count < max_resume {
                pending
                    .take()
                    .filter(|v| !v.is_empty())
                    .or(Some("Continue working on pending tasks.".to_string()))
            } else {
                None
            }
        };
        if let Some(context) = async_resume_context {
            if abort_signal.aborted() {
                break;
            }
            input = crate::config::input::from_str(config, &context, None);
            resume_count += 1;
            with_embeddings = true;
            continue;
        }

        // Done.
        break;
    }

    if abort_signal.aborted() {
        bail!("interrupted by user");
    }
    Config::run_post_turn_maintenance(config.clone());
    Ok(LoopResult::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MessageRole;
    use crate::utils::create_abort_signal;
    use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, TurnEvent};
    use harnx_hooks::{AsyncHookManager, PersistentHookManager};
    use parking_lot::RwLock;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    static SINK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    use tempfile::TempDir;

    fn replay_test_config(tmp: &TempDir) -> GlobalConfig {
        let mut config = Config::default();
        let mut session = crate::config::session::new(&config, "replay_test", None).unwrap();
        session.set_sessions_dir(tmp.path().to_path_buf());
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
            })
        });

        let ctx = make_test_context(global_config.clone(), call_fn, on_tool_round);

        let input = crate::config::input::from_str(&global_config, "do work", None);
        run_agent_loop(&ctx, input).await.unwrap();

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

    fn handoff_on_tool_round() -> OnToolRoundFn {
        Arc::new(move |_merged_input, results| {
            Box::pin(async move {
                let result = results
                    .first()
                    .expect("handoff test should produce single tool result");
                assert_eq!(result.call.name, "delegate-agent_session_handoff");
            })
        })
    }

    fn make_test_context(
        global_config: GlobalConfig,
        call_fn: AgentCallFn,
        on_tool_round: OnToolRoundFn,
    ) -> AgentLoopContext {
        AgentLoopContext {
            instance_id: harnx_core::instance::InstanceId::new(),
            config: global_config,
            abort_signal: create_abort_signal(),
            async_manager: Arc::new(tokio::sync::Mutex::new(AsyncHookManager::new())),
            persistent_manager: Arc::new(tokio::sync::Mutex::new(PersistentHookManager::new())),
            call_fn: Some(call_fn),
            on_tool_round: Some(on_tool_round),
            on_text_response: None,
            working_dir: None,
            initial_with_embeddings: false,
            initial_resume_count: 0,
            max_resume: Some(0),
            nats_hook_provider: None,
            pending_async_context: None,
            session_lock: None,
        }
    }

    fn handoff_event_matches_expected((event, source): &(AgentEvent, Option<AgentSource>)) -> bool {
        matches!(
            event,
            AgentEvent::Turn(TurnEvent::HandoffRequested { agent, session_id })
                if agent == "delegate-agent"
                    && session_id.as_deref() == Some("handoff-target-session")
        ) && source.as_ref().is_some_and(|source| {
            source.agent == "delegate-agent"
                && source.session_id.as_deref() == Some("handoff-target-session")
        })
    }

    /// Test that the dispatch path for _session_handoff tools returns a
    /// ToolResult.switch_agent that the loop detects. We inject a mock
    /// ToolProvider that returns the switch_agent JSON; the engine's
    /// detect_switch_agent picks it up.
    #[tokio::test(flavor = "multi_thread")]
    async fn handoff_returns_handoff_requested_and_emits_event() {
        let _guard = SINK_LOCK.lock().await;
        let _state_guard = crate::client::TestStateGuard::new(None).await;

        harnx_core::sink::clear_agent_event_sink();

        // Write a delegate-agent file so list_agents() discovers it.
        let tmp = TempDir::new().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("delegate-agent.md"),
            "---\nmodel: test-model\n---\nDelegate agent instructions\n",
        )
        .unwrap();

        // Set HARNX_CONFIG_DIR to our temp dir for agent discovery
        struct EnvGuard {
            key: &'static str,
            previous: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn set_path(key: &'static str, value: &std::path::Path) -> Self {
                let previous = std::env::var_os(key);
                unsafe { std::env::set_var(key, value) };
                Self { key, previous }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.previous {
                    Some(value) => unsafe { std::env::set_var(self.key, value) },
                    None => unsafe { std::env::remove_var(self.key) },
                }
            }
        }
        let _config_dir_guard = EnvGuard::set_path("HARNX_CONFIG_DIR", tmp.path());

        let mut config = crate::config::Config {
            model: crate::client::Model::new("test", "test-model"),
            sessions_dir_override: Some(tmp.path().join("sessions")),
            ..Default::default()
        };
        std::fs::create_dir_all(config.sessions_dir()).unwrap();
        let session = crate::config::session::new(&config, "handoff-session", None).unwrap();
        config.session = Some(session);
        config.set_use_tools(Some(vec!["delegate-agent_session_handoff".to_string()]));

        let global_config = Arc::new(RwLock::new(config));

        // Register our mock provider. We need to inject it into the tool
        // evaluation context. The providers are built by build_tool_providers
        // from Config.mcp_manager. For this test, we'll use the
        // handoff's built-in dispatch path which checks allowed_tool_names
        // and handoff_targets. The mock provider is an alternative but the
        // dispatch path for _session_handoff tools in dispatch_tool_call
        // synthesizes the switch_agent JSON directly when allowed_tool_names
        // contains the tool name.
        //
        // We need delegates-agent to be in handoff_targets. That's populated
        // by handoff_tool_declarations_for_agents which calls list_agents().
        // With HARNX_CONFIG_DIR set, list_agents() will find our test agent.
        //
        // However the problem is that build_tool_eval_context creates the
        // context from Config which doesn't have MCP managers. We need to
        // make the handoff tool be recognized. The dispatch path checks:
        // 1. Name ends with "_session_handoff"
        // 2. Name is in allowed_tool_names
        // 3. handoff_targets maps the bare name to target agent
        //
        // Since list_agents() will now find delegate-agent, handoff_targets
        // should contain "delegate-agent" -> "delegate-agent". We don't need
        // a mock provider — the dispatch path handles it.

        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();
        let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
            let cc = cc.clone();
            Box::pin(async move {
                let _n = cc.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "handoff now".to_string(),
                    None,
                    vec![ToolCall::new(
                        "delegate-agent_session_handoff".to_string(),
                        json!({
                            "prompt": "finish delegated work",
                            "session_id": "handoff-target-session"
                        }),
                        Some("handoff-call-1".to_string()),
                        None,
                    )],
                    CompletionTokenUsage::default(),
                ))
            })
        });

        let ctx = make_test_context(global_config.clone(), call_fn, handoff_on_tool_round());

        assert!(!global_config.read().is_compacting_session());
        let sink = Arc::new(CollectingSink::default());
        let input = crate::config::input::from_str(&global_config, "start handoff", None);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            harnx_core::sink::with_agent_event_sink(sink.clone(), async {
                run_agent_loop(&ctx, input).await
            }),
        )
        .await
        .expect("run_agent_loop timed out in handoff test")
        .unwrap();

        match result {
            LoopResult::HandoffRequested {
                agent,
                session_id,
                prompt,
            } => {
                assert_eq!(agent, "delegate-agent");
                assert_eq!(session_id.as_deref(), Some("handoff-target-session"));
                assert_eq!(prompt, "finish delegated work");
            }
            LoopResult::Completed => panic!("expected handoff result, got Completed"),
        }

        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(handoff_event_matches_expected));

        drop(events);
        harnx_core::sink::clear_agent_event_sink();
    }
}
