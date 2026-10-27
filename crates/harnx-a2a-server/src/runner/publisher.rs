//! Coalesced execution output becomes one CAS-owned event at a time.
use super::*;
use a2a_lf::TaskStatus;

mod follow;

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
    pub(super) suspending: bool,
    pub(super) resumed: bool,
    pub(super) replace_output: bool,
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
            suspending: false,
            resumed: false,
            replace_output: false,
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
        let (changes, response) = build_status_payload(record, &self.output, status);
        self.send(response, changes).await?;
        if self.record().task.status.state.is_terminal() {
            self.authority
                .project_terminal(&self.store, &self.session)
                .await?;
        }
        Ok(())
    }
    pub(super) async fn flush_artifact(&mut self, last: bool, replace: bool) -> Result<()> {
        let Some(update) =
            self.output
                .artifact_update(&self.record().task, last, replace || self.replace_output)
        else {
            return Ok(());
        };
        let response = StreamResponse::ArtifactUpdate(update);
        self.send(
            response,
            TaskChanges {
                artifacts: Some(vec![artifact(self.output.text.clone())]),
                ..Default::default()
            },
        )
        .await?;
        self.output.committed();
        self.replace_output = false;
        Ok(())
    }
    pub(super) async fn execute(&mut self, request: ExecutionRequest<'_>) -> Result<()> {
        let ExecutionRequest {
            session,
            input,
            cancel_rx,
            admitted_tx,
        } = request;
        if self.resumed {
            self.drain_outbox().await?;
            self.publish_snapshot();
        } else {
            self.set_status(TaskState::Working, None).await?;
        }
        let session = session.clone().with_external_admission().with_admission_id(
            self.authority
                .ticket(&self.storage_key)?
                .invocation_id()
                .into(),
        );
        self.follow_turn(
            &session,
            AdmittedTurn {
                input,
                cancel_rx,
                admitted_tx,
            },
        )
        .await
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

fn build_status_payload(
    record: &TaskRecord,
    output: &Output,
    status: TaskStatus,
) -> (TaskChanges, StreamResponse) {
    let history = status
        .message
        .as_ref()
        .filter(|_| status.state == TaskState::Completed)
        .map(|message| {
            let mut history = record.task.history.clone().unwrap_or_default();
            history.push(message.clone());
            history
        });
    let artifacts = if output.sent || !output.text.is_empty() {
        Some(vec![artifact(output.text.clone())])
    } else {
        record.task.artifacts.clone()
    };
    let changes = TaskChanges {
        status: Some(status.clone()),
        history,
        artifacts,
    };
    let response = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: record.task.id.clone(),
        context_id: record.task.context_id.clone(),
        status,
        metadata: None,
    });
    (changes, response)
}
