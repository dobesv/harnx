use super::*;
use harnx_execution_control::{CancelDisposition, CancelReceipt, CancelRequest, ExecutionStore};

impl SessionActor {
    pub(super) async fn answer_cancellation_command(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::Cancel {
                reply,
                expected_execution_id,
            } => self.answer_cancellation(reply, expected_execution_id).await,
            SessionCommand::AbandonCancellation {
                reply,
                expected_execution_id,
            } => {
                self.answer_cancellation_abandonment(reply, expected_execution_id)
                    .await
            }
            _ => unreachable!("non-cancellation command routed to cancellation handler"),
        }
    }

    pub(super) async fn answer_cancellation(
        &mut self,
        reply: tokio::sync::oneshot::Sender<Result<CancelReceipt, String>>,
        expected_execution_id: Option<String>,
    ) {
        let result = self
            .request_cancellation(expected_execution_id)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = reply.send(result);
    }

    pub(super) async fn answer_cancellation_abandonment(
        &mut self,
        reply: tokio::sync::oneshot::Sender<Result<CancelReceipt, String>>,
        expected_execution_id: String,
    ) {
        let result = self
            .abandon_cancellation(expected_execution_id)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = reply.send(result);
    }

    pub(super) async fn admit_prompt(
        &self,
        text: &str,
        options: &SessionPromptOptions,
    ) -> anyhow::Result<harnx_runtime::nats_session::AppendedPrompt> {
        let config = self.prompt_config();
        let input = build_input(&config, text, &options.attachment_refs)?;
        let source_dir = Config::session_attachments_dir(SessionAttachmentPath {
            agent_name: &self.key.agent,
            session_id: &self.key.session,
        });
        self.control_session()
            .await?
            .admit_input(&input, source_dir.as_deref())
            .await
    }

    async fn control_session(&self) -> anyhow::Result<NatsSession> {
        let abort = create_abort_signal();
        let config = self.prompt_config();
        let initializer = harnx_runtime::SessionInitializer::named_from_config(
            self.key.agent.clone(),
            &config.read(),
        );
        NatsSession::from_global_config(
            NatsSessionConfig {
                cluster: LOCAL_CLUSTER_KEY.into(),
                initializer,
                session_id: Some(self.key.session.clone()),
                // Durable cancellation acceptance must not wait for the local
                // worker-supervisor lock. Active owners observe the KV graph;
                // the shared activation is only a recovery wake-up fast path.
                activation_route: harnx_runtime::nats_worker::SessionActivationRoute::ClusterShared,
            },
            &config,
            abort,
        )
        .await
    }

    pub(super) async fn request_cancellation(
        &mut self,
        expected_execution_id: Option<String>,
    ) -> anyhow::Result<CancelReceipt> {
        if expected_execution_id.is_none() {
            self.abort_active_run();
        }
        // The injected test executor has no worker activation/control record.
        if self.actor_config.call_fn.is_some() {
            let mut receipt = CancelReceipt::idle();
            if self.active_run.is_some() {
                receipt.cancelled = true;
                receipt.disposition = CancelDisposition::Requested;
            }
            return Ok(receipt);
        }
        let receipt = self
            .control_session()
            .await?
            .request_cancel(CancelRequest {
                expected_execution_id,
                retry: true,
            })
            .await?;
        if receipt.cancelled {
            self.abort_active_run();
        }
        self.apply_cancellation(receipt.clone());
        Ok(receipt)
    }

    async fn abandon_cancellation(
        &mut self,
        expected_execution_id: String,
    ) -> anyhow::Result<CancelReceipt> {
        let receipt = self
            .control_session()
            .await?
            .abandon_unconfirmed_cancellation(&expected_execution_id)
            .await?;
        if receipt.abandoned {
            self.detach_active_run_for_abandonment().await;
            self.execution_id = receipt.execution_id.clone();
            self.execution_state = Some(harnx_execution_control::OperationState::Cancelled);
            self.apply_cancellation(receipt.clone());
        }
        Ok(receipt)
    }

    async fn detach_active_run_for_abandonment(&mut self) {
        self.pending.clear();
        let active_run = self.active_run.take();
        if let Some(run) = &active_run {
            run.abort_signal.set_ctrlc();
        }
        if let Some(mut task) = self.run_done_task.take() {
            task.abort();
            let _ = (&mut task).await;
        }
        // Awaiting the sole run task above guarantees that any completion it
        // sent is already queued. Discard it so it cannot later reset a fresh
        // replacement run to idle.
        while self.run_done_rx.try_recv().is_ok() {}
        if active_run.is_some() {
            let _ = self.broadcast_tx.send(Event::RunError(RunErrorEvent {
                base: base_event(),
                message: "Run abandoned; prior work may still be running".into(),
                code: None,
            }));
        }
    }

    fn abort_active_run(&mut self) {
        self.pending.clear();
        if let Some(run) = &self.active_run {
            run.abort_signal.set_ctrlc();
        }
    }

    pub(super) async fn refresh_cancellation(&mut self) {
        if self.actor_config.call_fn.is_some() {
            return;
        }
        match tokio::time::timeout(Duration::from_secs(2), self.read_cancellation()).await {
            Ok(Ok(Some(receipt))) => self.apply_cancellation(receipt),
            Ok(Ok(None)) => {}
            error => {
                log::debug!("cancellation hydration unavailable: {error:?}");
                if let SessionState::Cancelling(receipt)
                | SessionState::CancelUnconfirmed(receipt) = &self.state
                {
                    let mut receipt = receipt.clone();
                    receipt.disposition = CancelDisposition::Unconfirmed;
                    self.apply_cancellation(receipt);
                }
            }
        }
    }

    async fn read_cancellation(&mut self) -> anyhow::Result<Option<CancelReceipt>> {
        let js = self
            .actor_config
            .base_config
            .nats_jetstream(LOCAL_CLUSTER_KEY)
            .await?;
        let bucket = match js.get_key_value(harnx_execution_control::BUCKET).await {
            Ok(bucket) => bucket,
            Err(error) if harnx_runtime::nats_admin::kv_bucket_missing(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let store = ExecutionStore::from_store(bucket);
        let Some(current) = store.current(&self.key.session).await? else {
            return Ok(None);
        };
        let current = store.status(&current.reference).await?;
        self.execution_id = Some(current.reference.execution_id.clone());
        self.execution_state = Some(current.state);
        if current.cancellation.is_none() {
            return Ok(None);
        }
        Ok(Some(CancelReceipt::from_operation(&current, false)))
    }

    fn apply_cancellation(&mut self, receipt: CancelReceipt) {
        let next = match receipt.disposition {
            CancelDisposition::Requested
            | CancelDisposition::AlreadyRequested
            | CancelDisposition::Quiescing => SessionState::Cancelling(receipt.clone()),
            CancelDisposition::Unconfirmed => SessionState::CancelUnconfirmed(receipt.clone()),
            _ if matches!(
                self.state,
                SessionState::Cancelling(_) | SessionState::CancelUnconfirmed(_)
            ) =>
            {
                SessionState::Idle
            }
            _ => return,
        };
        if self.state == next {
            return;
        }
        self.state = next;
        let _ = self.broadcast_tx.send(Event::Custom(ag_ui_core::event::CustomEvent {
            base: base_event(), name: "cancellation_state".into(),
            value: serde_json::json!({ "session_id": self.key.session, "cancellation": receipt }),
        }));
    }
}
