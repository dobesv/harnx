//! Frontend follower ownership. Old task cleanup never blocks a replacement prompt.
use crate::types::{Tui, TuiEvent};
use anyhow::Result;
use harnx_render::pretty_error_string;

impl Tui {
    pub(crate) async fn start_prompt(&mut self, msg: crate::types::PendingMessage) -> Result<()> {
        self.retire_prompt_task();

        // Allocate a fresh abort signal for this task. Subsequent Ctrl+C
        // will signal exactly this task; later submissions get their
        // own fresh signal so that nothing in this branch can be
        // un-aborted by a future `abort_signal.reset()`.
        let new_abort = harnx_runtime::utils::create_abort_signal();
        self.current_prompt_abort = Some(new_abort.clone());
        self.live_events = self.live_events.fork();
        // This prompt receives its worker events directly through
        // TuiAgentEventSink. Pause the shared observer before activating the
        // worker so its advisory copy cannot be queued and rendered later.
        self.sync_session_activity_monitor();

        self.app.llm_busy = true;
        // Emit terminal status: prompt task starting.
        crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Working);
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;

        let (agent, cluster, session_id) = {
            let guard = self.config.read();
            let (agent, cluster) = guard.remote_agent.clone().unwrap_or_else(|| {
                (
                    guard
                        .agent
                        .as_ref()
                        .map(|agent| agent.name().to_string())
                        .unwrap_or_default(),
                    guard.default_cluster_key().to_string(),
                )
            });
            let session_id = guard.session.as_ref().map(|session| session.storage_key());
            (agent, cluster, session_id)
        };
        self.active_remote_session = session_id.map(|id| (id, cluster.clone()));

        let event_tx = self.event_tx.clone();

        let ctx = crate::prompt::PromptTaskContext {
            config: self.config.clone(),
            abort_signal: new_abort.clone(),
            live_events: self.live_events.clone(),
            #[cfg(test)]
            shared_pending_message: self.shared_pending_message.clone(),
            local_worker: self.local_worker.clone(),
            event_tx: event_tx.clone(),
            tool_confirmation_route: self.tool_confirmation_route.clone(),
        };

        let handle = Self::spawn_prompt_task(msg, ctx, (agent, cluster));
        self.current_prompt_handle = Some(handle);

        Ok(())
    }

    /// Abort this frontend's follower, not the worker. Durable cancellation must
    /// be accepted before composer reuse; generation/task fences reject its queue.
    pub(crate) fn retire_prompt_task(&mut self) {
        if let Some(abort) = self.current_prompt_abort.take() {
            abort.set_ctrlc();
        }
        if let Some(handle) = self.current_prompt_handle.take() {
            handle.abort();
            // Detached, not awaited here: reaping the handle is fire-and-forget
            // cleanup, and this call site cannot block on it without stalling
            // the frame that retired it.
            tokio::spawn(async move {
                let _ = handle.await;
            });
        }
    }

    fn spawn_prompt_task(
        msg: crate::types::PendingMessage,
        ctx: super::PromptTaskContext,
        route: (String, String),
    ) -> tokio::task::JoinHandle<()> {
        let (agent, cluster) = route;
        let event_tx = ctx.event_tx.clone();
        let new_abort = ctx.abort_signal.clone();
        tokio::spawn(async move {
            #[cfg(test)]
            let result: Result<()> = if cluster == harnx_runtime::config::LOCAL_CLUSTER_KEY {
                Self::run_test_prompt_task(msg, ctx).await
            } else {
                Self::run_nats_prompt_task(msg, ctx, agent, cluster).await
            };
            #[cfg(not(test))]
            let result: Result<()> = Self::run_nats_prompt_task(msg, ctx, agent, cluster).await;

            let error = match result {
                Err(_) if new_abort.aborted() => None,
                Err(err) => Some(pretty_error_string(&err)),
                Ok(()) => None,
            };
            let _ = event_tx.send(TuiEvent::PromptTaskFinished {
                task: new_abort,
                error,
            });
        })
    }
}
