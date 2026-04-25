use crate::types::Tui;
use crate::types::{PendingMessage, TuiEvent};
use anyhow::Result;
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use harnx_runtime::client::CompletionTokenUsage;
use harnx_runtime::config::{GlobalConfig, Input};
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
        input: Input,
        _resume_count: u32,
        _with_embeddings: bool,
    ) -> Result<()> {
        let _ = &ctx.pending_async_context;

        let call_fn: harnx_runtime::AgentCallFn = {
            let config = ctx.config.clone();
            Arc::new(
                move |input: &harnx_runtime::config::Input,
                      _config: &harnx_runtime::config::GlobalConfig,
                      abort: harnx_runtime::utils::AbortSignal| {
                    let input = input.clone();
                    let config = config.clone();
                    Box::pin(async move {
                        harnx_runtime::client::retry::call_with_retry_and_fallback_custom(
                            &input,
                            &config,
                            abort,
                            |inp, client, cfg, abort_signal| {
                                Box::pin(async move {
                                    if harnx_runtime::config::input::stream(inp, cfg) {
                                        Tui::call_chat_completions_streaming_tui(
                                            inp,
                                            client,
                                            cfg,
                                            abort_signal,
                                        )
                                        .await
                                    } else {
                                        harnx_runtime::client::call_chat_completions(
                                            inp,
                                            true,
                                            false,
                                            client,
                                            cfg,
                                            abort_signal,
                                        )
                                        .await
                                    }
                                })
                            },
                        )
                        .await
                    })
                },
            )
        };

        let event_tx = ctx.event_tx.clone();
        let shared_pending = ctx.shared_pending_message.clone();
        let on_tool_round: harnx_runtime::OnToolRoundFn = Arc::new(
            move |merged_input: &mut harnx_runtime::config::Input,
                  _tool_results: &[harnx_runtime::tool::ToolResult]| {
                let event_tx = event_tx.clone();
                let shared_pending = shared_pending.clone();
                Box::pin(async move {
                    let _ = event_tx.send(TuiEvent::ToolRoundComplete);
                    let mut guard = shared_pending.lock().await;
                    if let Some(pending) = guard.as_ref() {
                        let is_dot_command = pending.text.trim_start().starts_with('.');
                        let has_attachments = !pending.attachments.is_empty();
                        if !is_dot_command && !has_attachments {
                            let pending = guard.take().unwrap();
                            merged_input.set_injected_user_text(pending.text.clone());
                            let _ = event_tx.send(TuiEvent::PendingMessageConsumed(pending));
                        }
                    }
                })
            },
        );

        let event_tx = ctx.event_tx.clone();
        let on_text_response: harnx_runtime::OnTextResponseFn = Arc::new(
            move |output: String, usage: harnx_runtime::client::CompletionTokenUsage| {
                let event_tx = event_tx.clone();
                Box::pin(async move {
                    use harnx_core::event::{AgentEvent, ModelEvent};
                    let _ = event_tx.send(TuiEvent::Agent(
                        AgentEvent::Model(ModelEvent::Final { output, usage }),
                        None,
                    ));
                })
            },
        );

        let loop_ctx = harnx_runtime::AgentLoopContext {
            config: ctx.config.clone(),
            abort_signal: ctx.abort_signal.clone(),
            async_manager: ctx.async_manager.clone(),
            persistent_manager: ctx.persistent_manager.clone(),
            call_fn: Some(call_fn),
            on_tool_round: Some(on_tool_round),
            on_text_response: Some(on_text_response),
            initial_with_embeddings: true,
            initial_resume_count: 0,
            max_resume: None,
            pending_async_context: None,
        };

        harnx_runtime::run_agent_loop(&loop_ctx, input).await
    }

    async fn call_chat_completions_streaming_tui(
        input: &Input,
        client: &dyn harnx_runtime::client::Client,
        config: &GlobalConfig,
        abort_signal: AbortSignal,
    ) -> Result<(
        String,
        Option<String>,
        Vec<harnx_core::tool::ToolCall>,
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
            Ok(_) => Ok((text, thought, tool_calls, usage)),
            Err(err) => {
                if text.is_empty() {
                    Err(err)
                } else {
                    Ok((text, thought, vec![], usage))
                }
            }
        }
    }
}
