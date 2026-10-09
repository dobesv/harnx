//! Coalesced execution output becomes one CAS-owned event at a time.
use super::*;

pub(super) struct Publisher {
    pub(super) store: Arc<A2aStore>,
    pub(super) storage_key: String,
    pub(super) session: NatsSession,
    pub(super) events: broadcast::Sender<A2aEvent>,
    pub(super) record: Option<Arc<TaskRecord>>,
    pub(super) live: Arc<parking_lot::Mutex<LiveState>>,
    pub(super) output: Output,
    pub(super) admission_guard: Option<OwnedMutexGuard<Option<Arc<RunnerHandle>>>>,
    pub(super) authority: OwnedTask,
    pub(super) stop_observed: bool,
    #[cfg(feature = "fault-injection")]
    pub(super) hooks: Arc<crate::fault_injection::FaultHooks>,
}
impl Publisher {
    pub(super) fn new(
        store: Arc<A2aStore>,
        session: &NatsSession,
        channels: StreamChannels,
        admission: AdmissionOwnership,
    ) -> Self {
        let AdmissionOwnership {
            guard: admission_guard,
            authority,
        } = admission;
        Self {
            store,
            storage_key: session.storage_key().into(),
            session: session.clone(),
            events: channels.events,
            record: authority
                .context
                .document
                .state
                .active
                .as_ref()
                .map(|a| Arc::new(a.snapshot.clone())),
            live: channels.live,
            output: Output::default(),
            admission_guard: Some(admission_guard),
            authority,
            stop_observed: false,
            #[cfg(feature = "fault-injection")]
            hooks: Default::default(),
        }
    }
    pub(super) fn record(&self) -> &TaskRecord {
        self.record
            .as_deref()
            .expect("task persisted before admission")
    }
    pub(super) fn publish_snapshot(&mut self) {
        let mut live = self.live.lock();
        live.record = self.record.clone();
    }
    async fn send(&mut self, response: StreamResponse, changes: TaskChanges) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::Publication)
            .await;
        self.persist_event(changes, response).await?;
        let event = self
            .authority
            .flush_pending(&self.store, &self.storage_key)
            .await?
            .context("committed event missing")?;
        let mut live = self.live.lock();
        live.record = self.record.clone();
        let _ = self.events.send(event);
        Ok(())
    }

    pub(super) async fn set_status(&mut self, state: TaskState, text: Option<&str>) -> Result<()> {
        // Final text must be an ordered artifact event before immutable terminal.
        if state.is_terminal() && !self.output.pending.is_empty() {
            self.flush_artifact(true, true).await?;
        }
        let record = self.record();
        let status = status(state, text);
        let history = status
            .message
            .as_ref()
            .filter(|_| status.state == TaskState::Completed)
            .map(|message| {
                let mut history = record.task.history.clone().unwrap_or_default();
                history.push(message.clone());
                history
            });
        let changes = TaskChanges {
            status: Some(status.clone()),
            history,
            artifacts: if self.output.sent || !self.output.text.is_empty() {
                Some(vec![artifact(self.output.text.clone())])
            } else {
                record.task.artifacts.clone()
            },
        };
        let response = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: record.task.id.clone(),
            context_id: record.task.context_id.clone(),
            status,
            metadata: None,
        });
        self.send(response, changes).await?;
        if self.record().task.status.state.is_terminal() {
            self.authority
                .project_terminal(&self.store, &self.session)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn flush_artifact(&mut self, last: bool, replace: bool) -> Result<()> {
        if !last && self.output.pending.is_empty() {
            return Ok(());
        }
        let text = if replace {
            self.output.text.clone()
        } else {
            self.output.pending.clone()
        };
        let response = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: self.record().task.id.clone(),
            context_id: self.record().task.context_id.clone(),
            artifact: artifact(text),
            append: Some(self.output.sent && !replace),
            last_chunk: Some(last),
            metadata: None,
        });
        self.send(
            response,
            TaskChanges {
                artifacts: Some(vec![artifact(self.output.text.clone())]),
                ..Default::default()
            },
        )
        .await?;
        self.output.sent = true;
        self.output.pending.clear();
        Ok(())
    }
    pub(super) async fn execute(&mut self, request: ExecutionRequest<'_>) -> Result<()> {
        let ExecutionRequest {
            session,
            input,
            cancel_rx,
            admitted_tx,
        } = request;
        self.set_status(TaskState::Working, None).await?;
        let route = session
            .tool_confirmation_route(denial_confirmation_handler())
            .await?;
        let session = session.clone().with_external_admission().with_admission_id(
            self.authority
                .ticket(&self.storage_key)?
                .invocation_id()
                .into(),
        );
        let result = self
            .follow_turn(
                &session,
                &route,
                AdmittedTurn {
                    input,
                    cancel_rx,
                    admitted_tx,
                },
            )
            .await;
        route.close().await;
        result
    }

    async fn follow_turn(
        &mut self,
        session: &NatsSession,
        route: &harnx_runtime::nats_tool_confirmation::ToolConfirmationRoute,
        turn: AdmittedTurn,
    ) -> Result<()> {
        let AdmittedTurn {
            input,
            cancel_rx,
            admitted_tx,
        } = turn;
        let appended = self.admit_fixed_prompt(session, &input).await?;
        // Cancellation must not land before the admitted user message.
        self.admission_guard.take();
        let snapshot = self.live.lock().snapshot().expect("admitted snapshot");
        let _ = admitted_tx.send(Ok(snapshot));
        let limit = super::limits::task_output_limit(&self.store, &self.authority.context).await?;
        let inbox = Arc::new(super::inbox::TextInbox::new(limit));
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::Activation)
            .await;
        let follow = session.follow_admitted_prompt(
            appended,
            Arc::new(A2aEventSink(inbox.clone())),
            Some(cancel_rx),
            Some(route.subject()),
            RunTurnOptions::default(),
        );
        tokio::pin!(follow);
        let mut cancellation = tokio::time::interval(Duration::from_millis(250));
        let mut interval = tokio::time::interval(ARTIFACT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let result = loop {
            tokio::select! {
                result = &mut follow => break result?,
                _ = inbox.ready() => self.output.accept(inbox.take()?, limit)?,
                _ = interval.tick() => self.flush_artifact(false, false).await?,
                _ = cancellation.tick() => self.observe_cancel(session).await?,
            }
        };
        self.output.accept(inbox.take()?, limit)?;
        self.confirm_stop(session).await?;
        self.complete_turn(result).await
    }

    async fn complete_turn(&mut self, result: harnx_runtime::NatsTurnResult) -> Result<()> {
        let (state, text) = super::limits::bounded_outcome(
            turn_outcome(result, &self.output.text),
            super::limits::task_output_limit(&self.store, &self.authority.context).await?,
        );
        let replace = state == TaskState::Completed && text.as_deref() != Some(&self.output.text);
        if replace {
            self.output.text = text.clone().unwrap_or_default();
        }
        self.flush_artifact(true, replace).await?;
        self.set_status(state, text.as_deref()).await
    }
}
