//! Shared live and recovery dispatch. Target admission and prompt append are retry-stable.
use super::{handoff, handoff_recovery, NatsSessionLogBackend};
use crate::{
    config::GlobalConfig,
    fault_injection::{hit, RecoveryFaultPoint},
    nats_event_sink::NatsEventSink,
    nats_lease::NatsSessionLease,
    nats_worker::SessionActivationRoute,
    utils::AbortSignal,
    NatsSession, NatsSessionConfig,
};
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::{session::SessionLogEntry, tool::SwitchAgentData};
use std::sync::Arc;

pub(in crate::nats_worker) struct HandoffDispatchArgs<'a> {
    pub config: &'a GlobalConfig,
    pub source_session_id: &'a str,
    pub cluster_key: &'a str,
    pub jetstream_ctx: &'a jetstream::Context,
    pub activation_route: &'a SessionActivationRoute,
    pub replicas: usize,
    pub abort_signal: &'a AbortSignal,
    pub lease: Option<&'a NatsSessionLease>,
    pub event_sink: Option<&'a Arc<NatsEventSink>>,
}

impl HandoffDispatchArgs<'_> {
    pub(super) fn backend(&self) -> NatsSessionLogBackend {
        NatsSessionLogBackend::new(
            self.jetstream_ctx.clone(),
            self.source_session_id,
            self.replicas,
        )
    }

    async fn check_generation(&self, boundary: &str) -> Result<()> {
        anyhow::ensure!(!self.abort_signal.aborted(), "interrupted at {boundary}");
        if self
            .config
            .read()
            .run_context
            .as_ref()
            .is_some_and(|record| record.is_expired_at(chrono::Utc::now()))
        {
            return Err(crate::nats_session_metadata::run_limits::DeadlineExpired.into());
        }
        if let Some(lease) = self.lease {
            anyhow::ensure!(
                lease.revalidate_ownership().await?,
                "session lease lost at {boundary}"
            );
        }
        Ok(())
    }

    pub(super) async fn dispatch(&self, switch: SwitchAgentData) -> Result<()> {
        hit(
            self.source_session_id,
            RecoveryFaultPoint::HandoffDispatchBefore,
        )
        .await;
        self.check_generation("handoff-create").await?;
        let id = switch
            .tool_call_id
            .as_deref()
            .context("handoff requires durable tool-call identity")?;
        let target = handoff::NatsHandoffContext {
            config: self.config.clone(),
            source_session_id: self.source_session_id.to_owned(),
            cluster_key: self.cluster_key.to_owned(),
            jetstream_ctx: self.jetstream_ctx.clone(),
            activation_route: self.activation_route.clone(),
            replicas: self.replicas,
        }
        .resolve_target(&switch.agent, id)
        .await?;
        let (agent, session) = self.enqueue(target, &switch).await?;
        hit(
            self.source_session_id,
            RecoveryFaultPoint::HandoffTargetPromptAfter,
        )
        .await;
        self.emit_handoff_committed(&switch, agent, session).await
    }

    async fn enqueue(
        &self,
        target: handoff::HandoffTarget,
        switch: &SwitchAgentData,
    ) -> Result<(String, String)> {
        let session_id = target.resolve_session_id(switch.session_id.clone()).await?;
        let handoff::HandoffTarget {
            destination,
            initializer,
            parent,
            invocation_id,
            admitted_at,
        } = target;
        let target_session = NatsSession::new_with_resolved_replicas(
            NatsSessionConfig {
                cluster: destination.cluster,
                initializer,
                session_id: Some(session_id),
                activation_route: destination.activation_route,
            },
            destination.replicas,
            destination.client,
            destination.jetstream,
            self.abort_signal.clone(),
        )
        .await
        .with_context(|| format!("create handoff target session for '{}'", switch.agent))?
        .with_parent_cancel(tokio_util::sync::CancellationToken::new())
        .with_inherited_admission(
            parent,
            invocation_id,
            admitted_at,
            crate::nats_session_metadata::InvocationEdgeKind::Handoff,
            None,
        );
        self.check_generation("handoff-enqueue").await?;
        let enqueued = target_session
            .enqueue(&switch.prompt)
            .await
            .with_context(|| format!("queue handoff prompt for '{}'", switch.agent))?;
        log::debug!(
            "handoff queued: source_session_id={} target_agent={} target_session_id={} user_seq={}",
            self.source_session_id,
            switch.agent,
            enqueued.session_id,
            enqueued.user_msg_seq
        );
        Ok((destination.committed_agent, enqueued.session_id))
    }

    async fn emit_handoff_committed(
        &self,
        switch: &SwitchAgentData,
        agent: String,
        session: String,
    ) -> Result<()> {
        hit(
            self.source_session_id,
            RecoveryFaultPoint::HandoffCommittedBefore,
        )
        .await;
        self.check_generation("handoff-commit").await?;
        let backend = self.backend();
        let entries = backend.load_events_latest_async().await?;
        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
        let selection = handoff_recovery::selected_handoff(&effective)
            .context("handoff commit has no trusted selected round")?;
        anyhow::ensure!(
            selection.switch.tool_call_id == switch.tool_call_id,
            "handoff commit does not match selected round"
        );
        if handoff_recovery::committed_after(&effective, &selection) {
            return Ok(());
        }
        let entry = SessionLogEntry::HandoffCommitted {
            target_agent: agent,
            target_session_id: session,
            handoff_tool_call_id: switch.tool_call_id.clone(),
        };
        let after_seq = match self.lease {
            Some(lease) => {
                backend
                    .append_event_fenced_with_lease(
                        &entry,
                        lease,
                        entries.last().map_or(0, |(seq, _)| *seq),
                    )
                    .await?
            }
            None => backend.append_event(&entry).await?,
        };
        hit(
            self.source_session_id,
            RecoveryFaultPoint::HandoffCommittedAfter,
        )
        .await;
        self.notify(&switch.agent, entry, after_seq).await
    }

    async fn notify(
        &self,
        requested_agent: &str,
        entry: SessionLogEntry,
        after_seq: u64,
    ) -> Result<()> {
        use harnx_core::event::{AgentEvent, SessionEvent};
        let SessionLogEntry::HandoffCommitted {
            target_agent: agent,
            target_session_id: session,
            handoff_tool_call_id,
        } = entry
        else {
            anyhow::bail!("expected handoff commit notification");
        };
        let delivery_error = format!(
            "handoff target '{requested_agent}' session '{session}' was activated, but confirmation delivery failed; open that session manually",
        );
        let sink = match self.event_sink {
            Some(sink) => Arc::clone(sink),
            None => Arc::new(
                NatsEventSink::new(
                    self.jetstream_ctx.client().clone(),
                    self.jetstream_ctx.clone(),
                    self.source_session_id,
                )
                .await,
            ),
        };
        sink.publish_session_updated();
        sink.emit_required(AgentEvent::Session(SessionEvent::HandoffCommitted {
            agent,
            session_id: session,
            handoff_tool_call_id,
            after_seq: Some(after_seq),
        }));
        sink.flush().await.context(delivery_error)
    }
}
