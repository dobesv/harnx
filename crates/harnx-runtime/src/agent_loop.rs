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
                    .map(|s| s.id().to_string())
                    .unwrap_or_else(|| "default".to_string()),
                std::env::current_dir().unwrap_or_default(),
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
                // Emit a sourced Turn::Started so every front-end's sink
                // sees the agent change. The TUI's `render_ui_output_heading`
                // inserts a `> {agent} ▸ {session}` heading on source
                // changes; the CLI sink prints the same heading; the ACP
                // sink forwards it to `session_notification`. Without
                // this emit the unified loop silently switches agents
                // and the sub-agent's chunks render under the parent's
                // heading (the planner→executor e2e snapshot regression
                // and #312/#249 follow-up tests).
                let source = harnx_core::event::AgentSource {
                    agent: switch.agent.clone(),
                    session_id: switch.session_id.clone(),
                };
                harnx_core::sink::emit_agent_event_with_source(
                    harnx_core::event::AgentEvent::Turn(harnx_core::event::TurnEvent::Started),
                    Some(source),
                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MessageRole;
    use crate::utils::create_abort_signal;
    use harnx_hooks::{AsyncHookManager, PersistentHookManager};
    use parking_lot::RwLock;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

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

        // Build a Config with an attached session pointed at a temp dir.
        let mut config = Config::default();
        let mut session = crate::config::session::new(&config, "replay_test").unwrap();
        session.set_sessions_dir(tmp.path().to_path_buf());
        config.session = Some(session);
        let global_config = Arc::new(RwLock::new(config));

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

        let ctx = AgentLoopContext {
            config: global_config.clone(),
            abort_signal: create_abort_signal(),
            async_manager: Arc::new(tokio::sync::Mutex::new(AsyncHookManager::new())),
            persistent_manager: Arc::new(tokio::sync::Mutex::new(PersistentHookManager::new())),
            call_fn: Some(call_fn),
            on_tool_round: Some(on_tool_round),
            on_text_response: None,
            initial_with_embeddings: false,
            initial_resume_count: 0,
            max_resume: Some(0),
            pending_async_context: None,
        };

        let input = crate::config::input::from_str(&global_config, "do work", None);
        run_agent_loop(&ctx, input).await.unwrap();

        // The injection happened once; the bug would have made it appear in
        // session.messages once per round after the injection (here: 2x —
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
}
