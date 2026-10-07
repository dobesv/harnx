use super::*;
use harnx_runtime::nats_session::InterruptOutcome;

impl SessionActor {
    pub(super) async fn answer_interrupt(
        &mut self,
        reply: tokio::sync::oneshot::Sender<Result<InterruptOutcome, String>>,
    ) {
        let result = self
            .append_interrupt()
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = reply.send(result);
    }

    pub(super) async fn admit_prompt(
        &mut self,
        text: &str,
        options: &SessionPromptOptions,
    ) -> anyhow::Result<harnx_runtime::nats_session::AppendedPrompt> {
        let config = self.prompt_config().await;
        let input = build_input(&config, text, &options.attachment_refs)?;
        let source_dir = Config::session_attachments_dir(SessionAttachmentPath {
            agent_name: self.key.agent(),
            session_id: &self.key.session,
        });
        let session = self
            .control_session_with_user_id(options.user_id.as_deref())
            .await?;
        // Before admission, so the turn this prompt starts can already read it.
        self.record_web_session_url(&session, options).await;
        let session = match options.runtime_parent.as_deref() {
            Some(parent) => session.with_inherited_admission(
                parent.clone(),
                uuid::Uuid::new_v4().to_string(),
                chrono::Utc::now(),
                harnx_runtime::nats_session_metadata::InvocationEdgeKind::Handoff,
                None,
            ),
            None => session.with_external_admission().with_steering_admission(),
        };
        session.admit_input(&input, source_dir.as_deref()).await
    }

    pub(super) async fn control_session(&self) -> anyhow::Result<Option<NatsSession>> {
        let config = self.prompt_config().await.read().clone();
        let jetstream = crate::serve_nats_jetstream(&config, self.key.cluster()).await?;
        let store =
            harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
                .await?;
        // Registry presence only means a frontend attached, not that a prompt created metadata.
        if store
            .get_for_agent(self.key.session(), self.key.agent())
            .await?
            .is_none()
        {
            return Ok(None);
        }
        self.control_session_with_user_id(None).await.map(Some)
    }

    async fn control_session_with_user_id(
        &self,
        user_id: Option<&str>,
    ) -> anyhow::Result<NatsSession> {
        let abort = create_abort_signal();
        let config = self.prompt_config().await;
        let initializer = harnx_runtime::SessionInitializer::named_from_config(
            self.key.agent().to_string(),
            &config.read(),
        );
        let initializer = match user_id {
            Some(user_id) => initializer.with_user_id(user_id),
            None => initializer,
        };
        let cluster = self.key.cluster().to_string();
        NatsSession::from_global_config(
            NatsSessionConfig {
                cluster: cluster.clone(),
                initializer,
                session_id: Some(self.key.session.clone()),
                // The interrupt is one append to the session log and must not
                // wait for the local worker-supervisor lock. Active owners
                // watch their own session stream; the shared activation is only
                // a wake-up for a session whose worker is gone.
                activation_route: harnx_runtime::nats_worker::SessionActivationRoute::ClusterShared,
            },
            &config,
            abort,
        )
        .await
        .map_err(|error| crate::sanitize_nats_session_error(&cluster, error))
    }

    /// Local worker ids change across restarts, so a wind-up or resume
    /// activation addressed to a worker that is gone was dropped and nothing
    /// will retry it. A frontend attaching to the session republishes it, which
    /// is how an interrupted turn still winds up after this server restarted.
    pub(super) async fn republish_pending_activation(&self) {
        // A promptless subscription must not create metadata with defaults before
        // the first prompt supplies its request identity.
        if self.actor_config.call_fn.is_some() || self.session_base.is_none() {
            return;
        }
        match self.publish_pending_activation().await {
            Ok(republished) => log::debug!(
                "session attach: agent={} session_id={} republished_activation={republished}",
                self.key.agent(),
                self.key.session
            ),
            Err(error) => log::debug!(
                "session attach: agent={} session_id={} pending activation not republished: {error:#}",
                self.key.agent(),
                self.key.session
            ),
        }
    }

    async fn publish_pending_activation(&self) -> anyhow::Result<bool> {
        match self.control_session().await? {
            Some(session) => session.republish_pending_activation().await,
            None => Ok(false),
        }
    }

    /// Interrupt this session: abort whatever this server is running for it,
    /// then append one `Cancel` to the durable log. Acceptance is the append,
    /// so this returns as soon as the log has it — never waiting for a worker,
    /// a tool or a sub-agent to notice.
    pub(super) async fn append_interrupt(&mut self) -> anyhow::Result<InterruptOutcome> {
        self.abort_active_run();
        // The injected test executor runs in-process: there is no session log
        // to append a `Cancel` to, so an active run is the only thing an
        // interrupt can stop there. Sequence 0 stands for "no log entry" — a
        // durable log never hands one out.
        if self.actor_config.call_fn.is_some() {
            return Ok(match self.active_run.is_some() {
                true => InterruptOutcome::Accepted { cancel_seq: 0 },
                false => InterruptOutcome::Idle,
            });
        }
        let resumed = std::mem::replace(&mut self.state, SessionState::Interrupting);
        let outcome = self.interrupt_session().await;
        // Only an accepted `Cancel` names a state of its own. A failed append
        // restores what was there: the log is the authority, and the next
        // history refresh reads the truth off it — including an append that
        // landed after its acknowledgement was lost.
        self.state = match outcome.as_ref().ok().and_then(InterruptOutcome::cancel_seq) {
            Some(cancel_seq) => SessionState::Interrupted { cancel_seq },
            None => resumed,
        };
        outcome
    }

    async fn interrupt_session(&self) -> anyhow::Result<InterruptOutcome> {
        match self.control_session().await? {
            Some(session) => session.interrupt("user interrupt from web").await,
            None => Ok(InterruptOutcome::Idle),
        }
    }

    fn abort_active_run(&mut self) {
        self.pending.clear();
        if let Some(run) = &self.active_run {
            run.abort_signal.set_ctrlc();
        }
    }
}
