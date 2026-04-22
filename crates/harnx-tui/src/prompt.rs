use crate::types::Tui;
use crate::types::{PendingMessage, TuiEvent};
use anyhow::Result;
use harnx_hooks::{
    dispatch_hooks_with_count_and_manager, drain_async_results, inject_pending_async_context,
    AsyncHookManager, HookEvent, HookResultControl, PersistentHookManager,
};
use harnx_runtime::client::{call_chat_completions, CompletionTokenUsage};
use harnx_runtime::config::{Config, GlobalConfig, Input};
use harnx_runtime::tool::ToolResult;
use harnx_runtime::utils::AbortSignal;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub(super) struct PromptTaskContext {
    pub(super) config: GlobalConfig,
    pub(super) abort_signal: AbortSignal,
    pub(super) async_manager: Arc<Mutex<AsyncHookManager>>,
    pub(super) persistent_manager: Arc<Mutex<PersistentHookManager>>,
    pub(super) pending_async_context: Arc<Mutex<Option<String>>>,
    pub(super) shared_pending_message: Arc<Mutex<Option<PendingMessage>>>,
    pub(super) event_tx: mpsc::UnboundedSender<TuiEvent>,
}

impl Tui {
    pub(super) async fn run_prompt_task(msg: PendingMessage, ctx: PromptTaskContext) -> Result<()> {
        let attachment_dir = msg.attachment_dir.clone();
        let input_res = if msg.attachments.is_empty() {
            Ok(harnx_runtime::config::input::from_str(
                &ctx.config,
                &msg.text,
                None,
            ))
        } else {
            let paths: Vec<String> = msg
                .attachments
                .iter()
                .map(|a| a.path.to_string_lossy().to_string())
                .collect();
            harnx_runtime::config::input::from_files(&ctx.config, &msg.text, paths, None).await
        };
        if let Some(dir) = attachment_dir {
            let cleanup_dir = dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::types::cleanup_attachment_dir(&cleanup_dir);
            })
            .await;
        }
        let input = input_res?;
        Self::run_prompt_inner(ctx, input, 0, true).await
    }

    async fn run_prompt_inner(
        ctx: PromptTaskContext,
        mut input: Input,
        mut resume_count: u32,
        mut with_embeddings: bool,
    ) -> Result<()> {
        let mut pending_switch: Option<harnx_runtime::tool::SwitchAgentData> = None;
        loop {
            if input.is_empty() {
                break;
            }

            // Apply a deferred agent switch from the previous tool round.
            if let Some(switch) = pending_switch.take() {
                ctx.config.write().exit_agent()?;
                Config::use_agent(
                    &ctx.config,
                    &switch.agent,
                    switch.session_id.as_deref(),
                    ctx.abort_signal.clone(),
                )
                .await?;
                // Always empty the session on handoff so the new agent starts
                // fresh — the prior agent's system prompt and messages should
                // not bleed into the new agent's session (#291).
                if ctx.config.read().session.is_some() {
                    ctx.config.write().empty_session()?;
                }
                // Reset so the new agent starts fresh, matching the CMD
                // path which passes 0 when recursing after a handoff.
                resume_count = 0;
            }

            if with_embeddings {
                harnx_runtime::config::input::use_embeddings(
                    &mut input,
                    &ctx.config,
                    ctx.abort_signal.clone(),
                )
                .await?;
            }

            {
                let mut async_guard = ctx.async_manager.lock().await;
                let mut pending_guard = ctx.pending_async_context.lock().await;
                drain_async_results(&mut async_guard, &mut pending_guard);
                inject_pending_async_context(&mut input, &mut pending_guard);
            }

            ctx.config.write().before_chat_completion(&input)?;
            let (hooks, session_id, cwd) = Self::hook_dispatch_context(&ctx.config);
            let event = HookEvent::UserPromptSubmit {
                prompt: input.text().to_string(),
            };
            {
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
                    use harnx_core::event::{AgentEvent, ModelEvent};
                    let _ = ctx.event_tx.send(TuiEvent::Agent(
                        AgentEvent::Model(ModelEvent::Final {
                            output: String::new(),
                            usage: Default::default(),
                        }),
                        None,
                    ));
                    break;
                }
            }

            let llm_result = harnx_runtime::client::retry::call_with_retry_and_fallback_custom(
                &input,
                &ctx.config,
                ctx.abort_signal.clone(),
                move |input: &Input,
                      client: &dyn harnx_runtime::client::Client,
                      config: &GlobalConfig,
                      abort_signal| {
                    Box::pin(async move {
                        if harnx_runtime::config::input::stream(input, config) {
                            Self::call_chat_completions_streaming_tui(
                                input,
                                client,
                                config,
                                abort_signal,
                            )
                            .await
                        } else {
                            call_chat_completions(input, true, false, client, config, abort_signal)
                                .await
                        }
                    })
                },
            )
            .await;

            let (output, thought, tool_results, usage) = match llm_result {
                Ok(result) => result,
                Err(err) => {
                    // Persist the user message to the session log even on
                    // LLM failure so it is not lost.
                    let _ = ctx.config.write().after_chat_completion(
                        &input,
                        "",
                        None,
                        &[],
                        &Default::default(),
                    );
                    return Err(err);
                }
            };

            ctx.config.write().after_chat_completion(
                &input,
                &output,
                thought.as_deref(),
                &tool_results,
                &usage,
            )?;

            let stop_outcome = if tool_results.is_empty() {
                let event = HookEvent::Stop {
                    stop_hook_active: true,
                    last_assistant_message: Some(output.clone()),
                };
                let async_guard = ctx.async_manager.lock().await;
                Some(
                    dispatch_hooks_with_count_and_manager(
                        &event,
                        &hooks.entries,
                        &session_id,
                        &cwd,
                        resume_count,
                        Some(&async_guard),
                        Some(&ctx.persistent_manager),
                    )
                    .await,
                )
            } else {
                None
            };

            if !tool_results.is_empty() {
                let mut merged_input =
                    input.merge_tool_results(output, thought, tool_results.clone());
                let _ = ctx.event_tx.send(TuiEvent::ToolRoundComplete);
                // Defer agent switch to the top of the next iteration so the
                // TUI has a chance to render the tool-call row before the new
                // agent's streaming output begins.
                pending_switch = tool_results.iter().find_map(|v| v.switch_agent.clone());

                // Check if the user queued a message while tools were running.
                // If so, inject it as a trailing user message so the LLM sees
                // it right after the tool results.
                //
                // Skip dot-commands and messages with attachments — those need
                // the full submit_pending_message_inner() flow which runs on
                // the TUI side after LlmFinal.
                {
                    let mut guard = ctx.shared_pending_message.lock().await;
                    if let Some(pending) = guard.as_ref() {
                        let is_dot_command = pending.text.trim_start().starts_with('.');
                        let has_attachments = !pending.attachments.is_empty();
                        if !is_dot_command && !has_attachments {
                            let pending = guard.take().unwrap();
                            merged_input.set_injected_user_text(pending.text.clone());
                            let _ = ctx.event_tx.send(TuiEvent::PendingMessageConsumed(pending));
                        }
                    }
                }

                input = merged_input;
                with_embeddings = pending_switch.is_some();
                continue;
            } else {
                use harnx_core::event::{AgentEvent, ModelEvent};
                let _ = ctx.event_tx.send(TuiEvent::Agent(
                    AgentEvent::Model(ModelEvent::Final {
                        output: output.clone(),
                        usage: usage.clone(),
                    }),
                    None,
                ));
            }

            let max_resume = hooks.max_resume.unwrap_or(5);
            if let Some(stop_outcome) = stop_outcome {
                if stop_outcome.result.resume.unwrap_or(false) && resume_count < max_resume {
                    if ctx.abort_signal.aborted() {
                        break;
                    }
                    let context = stop_outcome
                        .result
                        .additional_context
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "Continue working on pending tasks.".to_string());
                    input = harnx_runtime::config::input::from_str(&ctx.config, &context, None);
                    resume_count += 1;
                    with_embeddings = true;
                    continue;
                }
            }

            let async_resume_context = {
                let mut async_guard = ctx.async_manager.lock().await;
                let mut pending_guard = ctx.pending_async_context.lock().await;
                if drain_async_results(&mut async_guard, &mut pending_guard)
                    && resume_count < max_resume
                {
                    pending_guard
                        .take()
                        .filter(|value: &String| !value.is_empty())
                        .or(Some("Continue working on pending tasks.".to_string()))
                } else {
                    None
                }
            };
            if let Some(context) = async_resume_context {
                if ctx.abort_signal.aborted() {
                    break;
                }
                input = harnx_runtime::config::input::from_str(&ctx.config, &context, None);
                resume_count += 1;
                with_embeddings = true;
                continue;
            }

            break;
        }

        Config::maybe_autoname_session(ctx.config.clone());
        Config::maybe_compact_session(ctx.config.clone());
        Ok(())
    }

    async fn call_chat_completions_streaming_tui(
        input: &Input,
        client: &dyn harnx_runtime::client::Client,
        config: &GlobalConfig,
        abort_signal: AbortSignal,
    ) -> Result<(
        String,
        Option<String>,
        Vec<ToolResult>,
        CompletionTokenUsage,
    )> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut handler = harnx_runtime::client::SseHandler::new(tx, abort_signal.clone());

        // Drain the SseEvent channel in the background so unbounded_send
        // doesn't fill up. No translation to TuiEvent happens here —
        // SseHandler emits AgentEvent::Model::{MessageChunk, ThoughtChunk}
        // via the global sink; TuiAgentEventSink forwards those directly
        // into the TuiEvent::Agent channel for render_agent_event to
        // handle.
        let drainer = tokio::spawn(async move {
            while rx.recv().await.is_some() {
                // discard — chunk flow goes through the sink.
            }
        });

        let (dry_run, user_agent) = {
            let cfg = config.read();
            (cfg.dry_run, cfg.user_agent.clone())
        };
        let call_ctx = harnx_runtime::client::ClientCallContext {
            user_agent: user_agent.as_deref(),
            dry_run,
        };
        let send_ret = harnx_runtime::client::chat_completions_streaming_with_input(
            client,
            input,
            config,
            &mut handler,
            &call_ctx,
        )
        .await;
        let aborted = handler.abort().aborted();
        let (text, thought, tool_calls, usage) = handler.take();
        let _ = drainer.await;

        if aborted {
            return Ok((text, thought, vec![], usage));
        }

        match send_ret {
            Ok(_) => Ok((
                text,
                thought,
                harnx_runtime::tool::eval_tool_calls(
                    &harnx_runtime::tool::build_tool_eval_context(config),
                    tool_calls,
                    &abort_signal,
                )?,
                usage,
            )),
            Err(err) => {
                if text.is_empty() {
                    Err(err)
                } else {
                    Ok((text, thought, vec![], usage))
                }
            }
        }
    }

    fn hook_dispatch_context(
        config: &GlobalConfig,
    ) -> (harnx_hooks::HooksConfig, String, std::path::PathBuf) {
        let config = config.read();
        (
            config.resolved_hooks(),
            config
                .session
                .as_ref()
                .map(|session| session.name())
                .unwrap_or("default")
                .to_string(),
            std::env::current_dir().unwrap_or_default(),
        )
    }
}
