//! Follow one original-ticket segment through its next durable response boundary.
use super::*;
use harnx_runtime::nats_session::AppendedPrompt;

struct SegmentOutput {
    inbox: Arc<crate::runner::inbox::TextInbox>,
    limit: usize,
}

struct SegmentFollow {
    appended: AppendedPrompt,
    cancel_rx: mpsc::Receiver<()>,
    after_request_seq: Option<u64>,
    output: SegmentOutput,
}

impl Publisher {
    async fn resolve_turn_prompt_handle(
        &mut self,
        session: &NatsSession,
        input: Option<harnx_core::input::Input>,
    ) -> Result<AppendedPrompt> {
        match input {
            Some(input) => self.admit_fixed_prompt(session, &input).await,
            None => {
                let ticket = self.authority.ticket(&self.storage_key)?;
                session
                    .fixed_prompt_handle(&ticket)
                    .await?
                    .context("resume fixed prompt missing")
            }
        }
    }

    pub(super) async fn follow_turn(
        &mut self,
        session: &NatsSession,
        turn: AdmittedTurn,
    ) -> Result<()> {
        let AdmittedTurn {
            input,
            cancel_rx,
            admitted_tx,
        } = turn;
        let appended = self.resolve_turn_prompt_handle(session, input).await?;
        let after_request_seq = self.follow_cursor()?;
        // Cancellation must not land before the admitted user message.
        self.admission_guard.take();
        let snapshot = self.live.lock().snapshot().expect("admitted snapshot");
        let _ = admitted_tx.send(Ok(snapshot));
        let limit =
            crate::runner::limits::task_output_limit(&self.store, &self.authority.context).await?;
        let inbox = Arc::new(crate::runner::inbox::TextInbox::new(limit));
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::Activation)
            .await;
        let result = self
            .follow_segment(
                session,
                SegmentFollow {
                    appended,
                    cancel_rx,
                    after_request_seq,
                    output: SegmentOutput {
                        inbox: inbox.clone(),
                        limit,
                    },
                },
            )
            .await?;
        self.finish_boundary(session, result, SegmentOutput { inbox, limit })
            .await
    }
    fn follow_cursor(&self) -> Result<Option<u64>> {
        if !self.resumed {
            return Ok(None);
        }
        let checkpoint = self
            .authority
            .context
            .document
            .state
            .active
            .as_ref()
            .and_then(|active| active.hitl_checkpoint.as_ref())
            .context("resume HITL checkpoint missing")?;
        Ok(Some(checkpoint.request.request_seq))
    }

    async fn follow_segment(
        &mut self,
        session: &NatsSession,
        segment: SegmentFollow,
    ) -> Result<FollowBoundary> {
        let SegmentFollow {
            appended,
            cancel_rx,
            after_request_seq,
            output,
        } = segment;
        let follow = session.follow_admitted_prompt_until_boundary(
            appended,
            Arc::new(A2aEventSink(output.inbox.clone())),
            FollowBoundaryOptions {
                after_request_seq,
                pending_cancel: Some(cancel_rx),
                ..Default::default()
            },
        );
        tokio::pin!(follow);
        let mut cancellation = tokio::time::interval(Duration::from_millis(250));
        let mut coalescer =
            crate::runner::coalescing::ArtifactCoalescer::new(output.inbox, output.limit);
        loop {
            tokio::select! {
                result = &mut follow => return result,
                ready = coalescer.ready(&mut self.output) => {
                    ready?;
                    self.flush_artifact(false, false).await?;
                }
                _ = cancellation.tick() => self.observe_cancel(session).await?,
            }
        }
    }

    async fn reconcile_resumed_terminal(
        &mut self,
        result: &mut harnx_runtime::NatsTurnResult,
    ) -> Result<()> {
        if !self.resumed {
            return Ok(());
        }
        self.reconcile_output(None).await?;
        if !result.was_cancelled && result.error.is_none() {
            result.response = Some(self.output.text.clone());
        }
        Ok(())
    }

    async fn finish_terminal_boundary(
        &mut self,
        session: &NatsSession,
        mut result: harnx_runtime::NatsTurnResult,
        output: SegmentOutput,
    ) -> Result<()> {
        self.output.accept(output.inbox.take()?, output.limit)?;
        self.reconcile_resumed_terminal(&mut result).await?;
        self.confirm_stop(session).await?;
        self.complete_turn(result).await
    }

    async fn finish_boundary(
        &mut self,
        session: &NatsSession,
        result: FollowBoundary,
        output: SegmentOutput,
    ) -> Result<()> {
        match result {
            FollowBoundary::InputRequired(pending) => {
                // Beyond this boundary, persistence/transport failure must not
                // interrupt the invocation or synthesize terminal A2A failure.
                self.suspending = true;
                self.output.accept(output.inbox.take()?, output.limit)?;
                self.suspend(pending).await
            }
            FollowBoundary::Terminal(result) => {
                self.finish_terminal_boundary(session, result, output).await
            }
        }
    }
}
