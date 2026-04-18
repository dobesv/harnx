use super::*;
use crate::config::Config;
use crate::tui::types::TuiEvent;

pub(super) struct PromptTaskContext {
    pub(super) config: GlobalConfig,
    pub(super) abort_signal: AbortSignal,
    pub(super) async_manager: Arc<Mutex<AsyncHookManager>>,
    pub(super) persistent_manager: Arc<Mutex<PersistentHookManager>>,
    pub(super) pending_async_context: Arc<Mutex<Option<String>>>,
    pub(super) shared_pending_message: Arc<Mutex<Option<crate::tui::types::PendingMessage>>>,
    pub(super) event_tx: mpsc::UnboundedSender<TuiEvent>,
}

impl Tui {
    pub(super) async fn run_prompt_task(
        msg: crate::tui::types::PendingMessage,
        ctx: PromptTaskContext,
    ) -> Result<()> {
        let attachment_dir = msg.attachment_dir.clone();
        let input_res = if msg.attachments.is_empty() {
            Ok(Input::from_str(&ctx.config, &msg.text, None))
        } else {
            let paths: Vec<String> = msg
                .attachments
                .iter()
                .map(|a| a.path.to_string_lossy().to_string())
                .collect();
            Input::from_files(&ctx.config, &msg.text, paths, None).await
        };
        if let Some(dir) = attachment_dir {
            let cleanup_dir = dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::tui::types::cleanup_attachment_dir(&cleanup_dir);
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
        let mut pending_switch: Option<crate::tool::SwitchAgentData> = None;
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
                // Reset so the new agent starts fresh, matching REPL/CMD
                // paths which pass 0 when recursing after a handoff.
                resume_count = 0;
            }

            if with_embeddings {
                input.use_embeddings(ctx.abort_signal.clone()).await?;
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
                    let _ =
                        ctx.event_tx
                            .send(TuiEvent::UiOutput(crate::ui_output::UiOutputEvent {
                                kind: crate::ui_output::UiOutputEventKind::LlmFinal {
                                    output: String::new(),
                                    usage: Default::default(),
                                },
                                source: None,
                            }));
                    break;
                }
            }

            let event_tx = ctx.event_tx.clone();
            let llm_result = crate::client::retry::call_with_retry_and_fallback_custom(
                &input,
                &ctx.config,
                ctx.abort_signal.clone(),
                |input: &Input, client: &dyn crate::client::Client, abort_signal| {
                    let event_tx = event_tx.clone();
                    Box::pin(async move {
                        if input.stream() {
                            Self::call_chat_completions_streaming_tui(
                                input,
                                client,
                                abort_signal,
                                event_tx,
                            )
                            .await
                        } else {
                            call_chat_completions(input, true, false, client, abort_signal).await
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
                let _ = ctx
                    .event_tx
                    .send(TuiEvent::UiOutput(crate::ui_output::UiOutputEvent {
                        kind: crate::ui_output::UiOutputEventKind::LlmFinal {
                            output: output.clone(),
                            usage: usage.clone(),
                        },
                        source: None,
                    }));
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
                    input = Input::from_str(&ctx.config, &context, None);
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
                input = Input::from_str(&ctx.config, &context, None);
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
        client: &dyn crate::client::Client,
        abort_signal: AbortSignal,
        event_tx: mpsc::UnboundedSender<TuiEvent>,
    ) -> Result<(
        String,
        Option<String>,
        Vec<ToolResult>,
        CompletionTokenUsage,
    )> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut handler = crate::client::SseHandler::new(tx, abort_signal.clone());

        let sender = tokio::spawn(async move {
            while let Some(evt) = rx.recv().await {
                match evt {
                    crate::client::SseEvent::Text(text) => {
                        let _ =
                            event_tx.send(TuiEvent::UiOutput(crate::ui_output::UiOutputEvent {
                                kind: crate::ui_output::UiOutputEventKind::MessageChunk {
                                    text,
                                    raw: None,
                                },
                                source: None,
                            }));
                    }
                    crate::client::SseEvent::Done => break,
                }
            }
        });

        let send_ret = client.chat_completions_streaming(input, &mut handler).await;
        let aborted = handler.abort().aborted();
        let (text, thought, tool_calls, usage) = handler.take();
        let _ = sender.await;

        if aborted {
            return Ok((text, thought, vec![], usage));
        }

        match send_ret {
            Ok(_) => Ok((
                text,
                thought,
                crate::tool::eval_tool_calls(client.global_config(), tool_calls)?,
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
    ) -> (crate::hooks::HooksConfig, String, std::path::PathBuf) {
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
