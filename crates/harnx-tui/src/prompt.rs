use crate::types::Tui;
use crate::types::{PendingMessage, TranscriptItem, TuiEvent};
use anyhow::{Context, Result};
#[cfg(test)]
use harnx_core::event::{AgentEvent, ModelEvent};
#[cfg(test)]
use harnx_core::sink::emit_agent_event;
use harnx_render::pretty_error_string;
#[cfg(test)]
use harnx_runtime::client::CompletionTokenUsage;
use harnx_runtime::config::GlobalConfig;
#[cfg(test)]
use harnx_runtime::config::Input;
use harnx_runtime::utils::AbortSignal;
use harnx_runtime::{NatsSession, NatsSessionConfig};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::agent_event_sink::TuiAgentEventSink;

pub(super) struct PromptTaskContext {
    pub(super) config: GlobalConfig,
    pub(super) abort_signal: AbortSignal,
    #[cfg(test)]
    pub(super) shared_pending_message: Arc<Mutex<Option<PendingMessage>>>,
    pub(super) local_worker:
        Arc<Mutex<Option<harnx_runtime::local_orchestrator::LocalWorkerSupervisor>>>,
    pub(super) event_tx: mpsc::UnboundedSender<TuiEvent>,
}

async fn run_nats_turn_with_tui_confirmation(
    session: &NatsSession,
    input: &harnx_runtime::config::Input,
    sink: Arc<dyn harnx_core::event::AgentEventSink>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
) -> Result<harnx_runtime::nats_session::NatsTurnResult> {
    let handler = crate::lifecycle::nats_tool_confirmation_handler(event_tx);
    session
        .run_turn_input_with_tool_confirmation(input, None, sink, None, handler)
        .await
}

#[cfg(test)]
fn test_tool_round_callback(ctx: &PromptTaskContext) -> harnx_runtime::OnToolRoundFn {
    let event_tx = ctx.event_tx.clone();
    let shared_pending = ctx.shared_pending_message.clone();
    Arc::new(move |merged_input, _tool_results| {
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
            Ok(())
        })
    })
}

impl Tui {
    /// Queue input submitted while the current turn is busy. Plain text for a
    /// durable session can reach the running worker's next tool-round seam;
    /// other input retains the frontend-owned next-turn fallback.
    pub(super) async fn queue_busy_input(&mut self, text: String) {
        let pending = PendingMessage {
            text,
            attachments: self.app.attachments.clone(),
            attachment_dir: self.app.attachment_dir.clone(),
            paste_count: self.app.paste_count,
        };
        self.app.pending_message = Some(pending.clone());

        match self.enqueue_pending_into_active_turn(&pending).await {
            Ok(true) => self.finish_durable_pending_enqueue(pending).await,
            Ok(false) => {
                *self.shared_pending_message.lock().await = Some(pending);
            }
            Err(error) => {
                log::warn!("failed to queue pending message into active turn: {error:#}");
                *self.shared_pending_message.lock().await = Some(pending);
            }
        }
        self.refresh_input_chrome();
    }

    async fn finish_durable_pending_enqueue(&mut self, pending: PendingMessage) {
        // The worker can consume this at its next tool-round seam. Remove the
        // local fallback so turn completion cannot submit it twice.
        self.app.pending_message = None;
        *self.shared_pending_message.lock().await = None;
        self.app.input = Self::new_input();
        self.app.transcript.push(TranscriptItem::UserText {
            text: pending.text,
            seq: None,
            timestamp: Some(chrono::Utc::now()),
        });
        self.pin_transcript_to_bottom();
    }

    async fn nats_session_for_target(
        &self,
        session_id: String,
        cluster: String,
    ) -> Result<NatsSession> {
        crate::remote_session::nats_session_for_target(
            &self.config,
            &self.local_worker,
            session_id,
            cluster,
        )
        .await
    }

    #[cfg(not(test))]
    pub(super) async fn activate_pending_session(
        &self,
        session_id: String,
        cluster: String,
    ) -> Result<()> {
        self.nats_session_for_target(session_id, cluster)
            .await?
            .activate_pending_turn()
            .await?;
        Ok(())
    }

    /// Queue a plain-text follow-up directly into the durable session while
    /// its current turn is still running. Dot commands and attachments retain
    /// the existing next-turn path because they require frontend processing.
    pub(super) async fn enqueue_pending_into_active_turn(
        &mut self,
        pending: &PendingMessage,
    ) -> Result<bool> {
        if pending.text.trim_start().starts_with('.') || !pending.attachments.is_empty() {
            return Ok(false);
        }
        let Some(target) = self.active_remote_session.clone() else {
            return Ok(false);
        };
        let (session_id, cluster) = target.clone();
        // Unit tests for the in-process loop use the shared pending-message
        // callback instead of a broker-backed local worker.
        #[cfg(test)]
        if cluster == harnx_runtime::config::LOCAL_CLUSTER_KEY {
            return Ok(false);
        }

        let session = self.nats_session_for_target(session_id, cluster).await?;
        let enqueued = session.enqueue_text(&pending.text).await?;
        if let Some(error) = enqueued.activation_error() {
            log::warn!(
                "queued user message durably at sequence {} but activation failed; retrying at turn completion: {error:#}",
                enqueued.user_msg_seq(),
            );
            self.retain_pending_remote_activation(target);
        }
        Ok(true)
    }

    /// Retain one durable session for a later activation-only retry.
    pub(super) fn retain_pending_remote_activation(&mut self, target: (String, String)) {
        self.pending_remote_activations.insert(target);
    }

    /// Move every retained target into the next retry batch.
    pub(super) fn take_pending_remote_activation_targets(
        &mut self,
    ) -> std::collections::HashSet<(String, String)> {
        std::mem::take(&mut self.pending_remote_activations)
    }

    async fn retry_pending_remote_activations(&mut self) {
        let targets = self.take_pending_remote_activation_targets();
        for (session_id, cluster) in targets {
            #[cfg(not(test))]
            if let Err(error) = self
                .activate_pending_session(session_id.clone(), cluster.clone())
                .await
            {
                log::warn!(
                    "failed to retry activation for pending durable session {session_id}: {error:#}"
                );
                self.retain_pending_remote_activation((session_id, cluster));
            }

            #[cfg(test)]
            let _ = (session_id, cluster);
        }
    }

    pub(super) async fn finish_prompt_task(&mut self, task: AbortSignal, error: Option<String>) {
        let is_current = self
            .current_prompt_abort
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &task));
        if !is_current {
            return;
        }

        self.current_prompt_abort = None;
        if let Some(error) = error {
            self.finish_main_prompt_error(error).await;
        }
        // Turn lifecycle advisories are lossy by design. Normally Turn::Ended
        // already completed the UI state; task exit is the local owner's
        // authoritative fallback when that advisory was missed or setup failed
        // before a worker could start the turn.
        self.complete_main_prompt().await;
    }

    pub(super) async fn complete_main_prompt(&mut self) {
        self.current_prompt_abort = None;
        self.app.llm_busy = false;
        self.active_remote_session = None;
        *self.shared_pending_message.lock().await = None;
        self.app.last_ui_output_source = None;
        self.refresh_input_chrome();
        self.retry_pending_remote_activations().await;

        if let Some(pending) = self.app.pending_message.take() {
            if let Err(err) = self.submit_pending_message(pending).await {
                self.app
                    .transcript
                    .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
                self.pin_transcript_to_bottom();
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn run_test_prompt_task(
        msg: PendingMessage,
        ctx: PromptTaskContext,
    ) -> Result<()> {
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

    #[cfg(test)]
    async fn run_prompt_inner(
        ctx: PromptTaskContext,
        input: Input,
        _resume_count: u32,
        _with_embeddings: bool,
    ) -> Result<()> {
        let call_fn: harnx_runtime::AgentCallFn = {
            let config = ctx.config.clone();
            Arc::new(
                move |input: &mut harnx_runtime::config::Input,
                      _config: &harnx_runtime::config::GlobalConfig,
                      abort: harnx_runtime::utils::AbortSignal| {
                    let config = config.clone();
                    Box::pin(async move {
                        harnx_runtime::client::retry::call_with_retry_and_fallback_custom(
                            input,
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

        let on_tool_round = test_tool_round_callback(&ctx);

        let event_tx = ctx.event_tx.clone();
        let on_text_response: harnx_runtime::OnTextResponseFn = Arc::new(
            move |output: String, usage: harnx_runtime::client::CompletionTokenUsage| {
                let event_tx = event_tx.clone();
                Box::pin(async move {
                    use harnx_core::event::{AgentEvent, ModelEvent};
                    let _ = event_tx.send(TuiEvent::Agent(AgentEvent::Model(ModelEvent::Final {
                        output,
                        usage,
                    })));
                })
            },
        );
        let loop_ctx = harnx_runtime::AgentLoopContext {
            instance_id: harnx_core::instance::ServerScope::new(),
            config: ctx.config.clone(),
            abort_signal: ctx.abort_signal.clone(),
            call_fn: Some(call_fn),
            on_tool_round: Some(on_tool_round),
            on_text_response: Some(on_text_response),
            initial_with_embeddings: true,
            initial_resume_count: 0,
            max_resume: None,
            nats_hook_provider: None,
            pending_async_context: None,
            working_dir: None,
        };

        harnx_runtime::run_agent_loop_with_local_handoff(&loop_ctx, input).await
    }

    /// Drive either a local or configured remote agent through NATS.
    pub(super) async fn run_nats_prompt_task(
        msg: PendingMessage,
        ctx: PromptTaskContext,
        agent: String,
        cluster: String,
    ) -> Result<()> {
        let input_res = if msg.attachments.is_empty() {
            Ok(harnx_runtime::config::input::from_str(
                &ctx.config,
                &msg.text,
                None,
            ))
        } else {
            let paths = msg
                .attachments
                .iter()
                .map(|attachment| attachment.path.to_string_lossy().to_string())
                .collect();
            harnx_runtime::config::input::from_files(&ctx.config, &msg.text, paths, None).await
        };
        if let Some(dir) = msg.attachment_dir.clone() {
            let _ = tokio::task::spawn_blocking(move || {
                crate::types::cleanup_attachment_dir(&dir);
            })
            .await;
        }
        let input = input_res?;

        let activation_route = harnx_runtime::local_orchestrator::activation_route_for_cluster(
            &cluster,
            &ctx.local_worker,
            ctx.abort_signal.clone(),
        )
        .await?;

        let sink = Arc::new(TuiAgentEventSink::new(ctx.event_tx.clone()));
        let session_id = ctx
            .config
            .read()
            .session
            .as_ref()
            .map(|session| session.id().to_string());
        let initializer = {
            let config = ctx.config.read();
            harnx_runtime::SessionInitializer::named_from_config(agent, &config)
        };
        let session = NatsSession::from_global_config(
            NatsSessionConfig {
                cluster: cluster.clone(),
                initializer,
                session_id,
                activation_route,
            },
            &ctx.config,
            ctx.abort_signal.clone(),
        )
        .await
        .context("failed to create NATS session")?;

        let result =
            run_nats_turn_with_tui_confirmation(&session, &input, sink, ctx.event_tx.clone())
                .await?;
        harnx_runtime::commands::update_last_message_after_nats_turn(&ctx.config, input, &result);
        log::info!(
            "prompt completed: cluster={} session_id={} cancelled={}",
            cluster,
            result.session_id,
            result.was_cancelled
        );
        Ok(())
    }

    #[cfg(test)]
    async fn call_chat_completions_streaming_tui(
        input: &mut Input,
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
                if text.trim().is_empty() {
                    Err(err)
                } else {
                    emit_agent_event(AgentEvent::Model(ModelEvent::Error(pretty_error_string(
                        &err,
                    ))));
                    Ok((text, thought, vec![], usage))
                }
            }
        }
    }
}
