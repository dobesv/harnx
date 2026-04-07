use super::*;
use crate::config::Config;
use crate::tui::types::TuiEvent;

pub(super) struct PromptTaskContext {
    pub(super) config: GlobalConfig,
    pub(super) abort_signal: AbortSignal,
    pub(super) async_manager: Arc<Mutex<AsyncHookManager>>,
    pub(super) persistent_manager: Arc<Mutex<PersistentHookManager>>,
    pub(super) pending_async_context: Arc<Mutex<Option<String>>>,
    pub(super) event_tx: mpsc::UnboundedSender<TuiEvent>,
}

impl Tui {
    pub(super) async fn run_prompt_task(
        config: GlobalConfig,
        text: String,
        attachments: Vec<crate::tui::types::Attachment>,
        attachment_dir: Option<std::path::PathBuf>,
        abort_signal: AbortSignal,
        async_manager: Arc<Mutex<AsyncHookManager>>,
        persistent_manager: Arc<Mutex<PersistentHookManager>>,
        pending_async_context: Arc<Mutex<Option<String>>>,
        event_tx: mpsc::UnboundedSender<TuiEvent>,
    ) -> Result<()> {
        let input = if attachments.is_empty() {
            Input::from_str(&config, &text, None)
        } else {
            let paths: Vec<String> = attachments
                .iter()
                .map(|a| a.path.to_string_lossy().to_string())
                .collect();
            Input::from_files(&config, &text, paths, None).await?
        };
        // Clean up attachment directory now that Input has read the files
        if let Some(dir) = attachment_dir {
            crate::tui::types::cleanup_attachment_dir(&dir);
        }
        Self::run_prompt_inner(
            PromptTaskContext {
                config,
                abort_signal,
                async_manager,
                persistent_manager,
                pending_async_context,
                event_tx,
            },
            input,
            0,
            true,
        )
        .await
    }

    async fn run_prompt_inner(
        ctx: PromptTaskContext,
        mut input: Input,
        mut resume_count: u32,
        mut with_embeddings: bool,
    ) -> Result<()> {
        loop {
            if input.is_empty() {
                break;
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

            let client = input.create_client()?;
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
                    let _ = ctx.event_tx.send(TuiEvent::Finished {
                        output: String::new(),
                        usage: Default::default(),
                    });
                    break;
                }
            }

            let (output, thought, tool_results, usage) = if !input.stream() {
                call_chat_completions(
                    &input,
                    true,
                    false,
                    client.as_ref(),
                    ctx.abort_signal.clone(),
                )
                .await?
            } else {
                Self::call_chat_completions_streaming_tui(
                    &input,
                    client.as_ref(),
                    ctx.abort_signal.clone(),
                    ctx.event_tx.clone(),
                )
                .await?
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
                let switch_agent = tool_results.iter().find_map(|v| v.switch_agent.clone());
                if let Some(switch_agent) = switch_agent {
                    ctx.config.write().exit_agent()?;
                    Config::use_agent(
                        &ctx.config,
                        &switch_agent.agent,
                        None,
                        ctx.abort_signal.clone(),
                    )
                    .await?;
                }
                let _ = ctx.event_tx.send(TuiEvent::ToolRoundComplete {
                    tool_count: tool_results.len(),
                });
                input = input.merge_tool_results(output, thought, tool_results);
                with_embeddings = false;
                continue;
            } else {
                let _ = ctx.event_tx.send(TuiEvent::Finished {
                    output: output.clone(),
                    usage: usage.clone(),
                });
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
        Config::maybe_compress_session(ctx.config.clone());
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
                        let _ = event_tx.send(TuiEvent::Chunk(text));
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
