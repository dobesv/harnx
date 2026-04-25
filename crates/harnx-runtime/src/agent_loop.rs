//! Unified agent loop — `run_agent_loop` is the single canonical
//! implementation of the LLM-call → tool-round → merge → repeat cycle
//! that every front-end (CLI, TUI, ACP server) drives through.
//!
//! Previously this logic lived in three places:
//! - `harnx-runtime/src/commands.rs::ask_inner` (CLI)
//! - `harnx-tui/src/prompt.rs::run_prompt_inner` (TUI)
//! - `harnx-acp-server/src/lib.rs::HarnxAgent::prompt` (ACP server)
//!
//! Those diverged over time; the ACP variant had a bug (#305) where
//! recoverable tool errors ended the session instead of being fed back
//! to the LLM. This module provides the canonical loop that all three
//! front-ends now delegate to.

use crate::{
    config::{Config, GlobalConfig, Input},
    tool::{execute_tool_round, ToolResult},
    utils::dimmed_text,
};
use anyhow::{bail, Result};
use harnx_hooks::{
    dispatch_hooks_with_count_and_manager, dispatch_hooks_with_managers, drain_async_results,
    inject_pending_async_context, AsyncHookManager, HookEvent, HookResultControl,
    PersistentHookManager,
};
use std::{future::Future, pin::Pin, sync::Arc};

use crate::client::CompletionTokenUsage;
use crate::client::retry::call_with_retry_and_fallback;
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
            &'a Input,
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
/// ACP server `LocalSet`.
pub struct AgentLoopContext {
    pub config: GlobalConfig,
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
    pub pending_async_context: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
}

/// Run the canonical agent loop.
///
/// Executes: embeddings → async-hook drain → `before_chat_completion` →
/// `UserPromptSubmit` hook → LLM call (with retry/fallback) → tool round
/// (if tool calls) → persist → stop hook → resume / agent switch / done.
/// Repeats until no tool results and no resume signal.
///
/// On clean exit returns `Ok(())`. On LLM error dispatches `StopFailure`
/// hook and propagates. On fatal tool error propagates. Recoverable tool
/// errors are already converted to `{"is_error":true}` results by
/// `execute_tool_round` and fed back to the LLM.
pub async fn run_agent_loop(ctx: &AgentLoopContext, initial_input: Input) -> Result<()> {
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
                    .map(|s| s.name().to_string())
                    .unwrap_or_else(|| "default".to_string()),
                std::env::current_dir().unwrap_or_default(),
            )
        };

        let max_resume = ctx.max_resume.unwrap_or_else(|| hooks.max_resume.unwrap_or(5));

        // Dispatch UserPromptSubmit hook (was previously TUI-only; now unified).
        {
            let event = HookEvent::UserPromptSubmit {
                prompt: input.text().to_string(),
            };
            let async_guard = ctx.async_manager.lock().await;
            let outcome = dispatch_hooks_with_count_and_manager(
                &event,
                &hooks.entries,
                &session_id,
                &cwd,
                resume_count,
                Some(&async_guard),
                Some(&ctx.persistent_manager),
            )
            .await;
            if matches!(outcome.control, HookResultControl::Block { .. }) {
                break;
            }
        }

        // LLM call (with retry + fallback).
        let llm_result = if let Some(ref call_fn) = ctx.call_fn {
            call_fn(&input, config, abort_signal.clone()).await
        } else {
            // Use the default call function, which respects config.stream:
            // streaming (call_chat_completions_streaming) when enabled, or
            // non-streaming (call_chat_completions) otherwise. Critically,
            // call_chat_completions_streaming does NOT write to stdout, which
            // matters for ACP server mode where stdout is the JSON-RPC
            // transport. The old hardcoded call_chat_completions(inp, true, ...)
            // always printed to stdout, corrupting the ACP connection.
            call_with_retry_and_fallback(&input, config, abort_signal.clone()).await
        };

        let (output, thought, tool_calls, usage) = match llm_result {
            Ok(result) => result,
            Err(err) => {
                // LLM error — dispatch StopFailure hook and persist empty turn.
                let event = HookEvent::StopFailure {
                    error: err.to_string(),
                    error_type: "api_error".to_string(),
                };
                {
                    let async_guard = ctx.async_manager.lock().await;
                    let _ = dispatch_hooks_with_managers(
                        &event,
                        &hooks.entries,
                        &session_id,
                        &cwd,
                        Some(&async_guard),
                        Some(&ctx.persistent_manager),
                    )
                    .await;
                }
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
                config,
                &input,
                &output,
                thought.as_deref(),
                tool_calls,
                abort_signal,
            )?
        };

        // Emit status/usage line for text-only turns. CLI-only: fires when
        // no on_text_response callback is set. TUI and ACP handle their own
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
            let async_guard = ctx.async_manager.lock().await;
            let outcome = dispatch_hooks_with_count_and_manager(
                &event,
                &hooks.entries,
                &session_id,
                &cwd,
                resume_count,
                Some(&async_guard),
                Some(&ctx.persistent_manager),
            )
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
                config.write().exit_agent()?;
                Config::use_agent(
                    config,
                    &switch.agent,
                    switch.session_id.as_deref(),
                    abort_signal.clone(),
                )
                .await?;
                // Empty session so new agent starts fresh (see #291).
                if config.read().session.is_some() {
                    config.write().empty_session()?;
                }
                // Rebuild input from the handoff prompt (#303).
                input = crate::config::input::from_str(config, &switch.prompt, None);
                resume_count = 0;
                with_embeddings = true;
                continue;
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
    Config::maybe_autoname_session(config.clone());
    Config::maybe_compact_session(config.clone());
    Ok(())
}
