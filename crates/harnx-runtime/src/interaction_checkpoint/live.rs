//! Live durable boundaries; crash classification belongs to recovery.
use super::{records, store::CheckpointStore, RoundStart};
use crate::{
    config::{session_persistence::active_session_sink, GlobalConfig},
    nats_lease::NatsSessionLease,
    nats_worker::NatsSessionLogBackend,
};
use anyhow::{ensure, Context, Result};
use harnx_core::{
    session::SessionLogEntry,
    tool::{ToolCall, ToolResult},
};
use harnx_engine::tool::{ToolEvalContext, ToolRoundOutcome};
use std::sync::Arc;

/// Lease-owning adapter carried by the session's persistence sink.
#[derive(Clone)]
pub struct InteractionSession {
    pub(crate) backend: NatsSessionLogBackend,
    pub(crate) lease: Arc<NatsSessionLease>,
    pub(crate) replicas: usize,
}

impl InteractionSession {
    pub fn new(
        backend: NatsSessionLogBackend,
        lease: Arc<NatsSessionLease>,
        replicas: usize,
    ) -> Self {
        Self {
            backend,
            lease,
            replicas,
        }
    }

    pub(crate) async fn append(&self, entry: &SessionLogEntry) -> Result<u64> {
        let entries = self.backend.load_events_latest_async().await?;
        ensure!(
            self.lease.revalidate_ownership().await?,
            "session lease lost before interaction append"
        );
        self.backend
            .append_event_fenced_with_lease(
                entry,
                &self.lease,
                entries.last().map_or(0, |(seq, _)| *seq),
            )
            .await
    }
}

// qual:allow(srp, god_struct) reason: "One lease-scoped round state is shared by the completion, attempt and suspension modules; fields belong to the same replay boundary."
#[derive(Clone)]
pub(crate) struct DurableRound {
    pub session: InteractionSession,
    pub store: CheckpointStore,
    pub config: GlobalConfig,
    pub seq: u64,
    pub calls: Vec<ToolCall>,
    pub chain_version: String,
    started: Arc<tokio::sync::OnceCell<()>>,
}

impl DurableRound {
    pub async fn open(
        config: &GlobalConfig,
        seq: Option<u64>,
        calls: &[ToolCall],
    ) -> Result<Option<Self>> {
        Self::open_inner(config, seq, calls, false).await
    }

    pub(crate) async fn open_recovery(
        config: &GlobalConfig,
        seq: u64,
        calls: &[ToolCall],
    ) -> Result<Self> {
        Self::open_inner(config, Some(seq), calls, true)
            .await?
            .context("recovery requires a fenced NATS session")
    }

    // qual:allow(iosp) reason: "Select fenced session support and the legacy/no-chain fallback before opening the async checkpoint store."
    async fn open_inner(
        config: &GlobalConfig,
        seq: Option<u64>,
        calls: &[ToolCall],
        recovery: bool,
    ) -> Result<Option<Self>> {
        if config.read().dry_run {
            return Ok(None);
        }
        let Some(session) =
            active_session_sink(config, None).and_then(|sink| sink.interaction_session())
        else {
            return Ok(None);
        };
        let seq = seq.context("durable tool round has no sequence")?;
        let chain = config.read().resolved_middleware()?;
        if !recovery && chain.entries.is_empty() {
            // Worker preflight classifies orphaned records before continuation.
            // Restore those slots even after middleware removal. If ToolCalls
            // is still the tail, no interaction records can follow it; only the
            // checkpoint-only ownership window needs probing. Keep the full
            // read for resumed rounds whose tail has moved, including when
            // checkpoint data was lost but log records still prove ownership.
            // crates/harnx-runtime/tests/it/nats_tool_provider/tool_round/no_middleware.rs bounds
            // session-stream requests per round; keep this path O(1) in
            // session history length.
            let entries = match session.backend.last_entry_async().await? {
                Some((tail, SessionLogEntry::ToolCalls { .. })) if tail == seq => Vec::new(),
                _ => session.backend.load_events_latest_async().await?,
            };
            if !super::guard::interaction_record_for_round(&entries, seq)
                && !super::round_started(
                    &session.backend.jetstream(),
                    session.backend.session_id(),
                    seq,
                )
                .await?
            {
                return Ok(None);
            }
        }
        ensure!(
            session.lease.revalidate_ownership().await?,
            "session lease lost before round-start"
        );
        let store = CheckpointStore::ensure(&session.backend.jetstream(), session.replicas).await?;
        // Legacy approval can defer before middleware is entered. Don't claim
        // ownership of that round until an authorized call reaches this driver.
        let chain_version = chain.version;
        Ok(Some(Self {
            session,
            store,
            config: config.clone(),
            seq,
            calls: calls.to_vec(),
            chain_version,
            started: Arc::new(tokio::sync::OnceCell::new()),
        }))
    }

    pub(super) async fn ensure_started(&self) -> Result<()> {
        self.started
            .get_or_try_init(|| async {
                ensure!(
                    self.session.lease.revalidate_ownership().await?,
                    "session lease lost before round-start"
                );
                crate::fault_injection::hit(
                    self.session.backend.session_id(),
                    crate::fault_injection::RecoveryFaultPoint::RoundStartBefore,
                )
                .await;
                self.store
                    .start_round(&RoundStart {
                        version: 1,
                        session_storage_key: self.session.backend.session_id().into(),
                        tool_round_seq: self.seq,
                        fence_token: self.session.lease.fence_token(),
                    })
                    .await?;
                crate::fault_injection::hit(
                    self.session.backend.session_id(),
                    crate::fault_injection::RecoveryFaultPoint::RoundStartAfter,
                )
                .await;
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    pub fn prefix(&self, call: &ToolCall) -> Result<String> {
        Ok(records::call_prefix(
            self.session.backend.session_id(),
            self.seq,
            call.id
                .as_deref()
                .context("durable tool call has no identity")?,
        ))
    }

    pub fn install(&self, ctx: &mut ToolEvalContext) {
        let render = ctx.clone();
        let round = self.clone();
        ctx.on_tool_call_completed = Some(Arc::new(move |result| {
            let round = round.clone();
            let render = render.clone();
            Box::pin(async move { round.complete(result, &render).await })
        }));
        let round = self.clone();
        ctx.on_interaction = Some(Arc::new(move |suspension| {
            let round = round.clone();
            Box::pin(async move { round.suspend(suspension).await })
        }));
        ctx.middleware_fn = Some(self.middleware(ctx.middleware_fn.clone()));
    }

    pub async fn evaluate(
        &self,
        ctx: &ToolEvalContext,
        calls: Vec<ToolCall>,
        abort: &harnx_core::abort::AbortSignal,
    ) -> Result<Vec<ToolResult>> {
        loop {
            crate::fault_injection::hit(
                self.session.backend.session_id(),
                crate::fault_injection::RecoveryFaultPoint::RerunBefore,
            )
            .await;
            let completed = self
                .completed()
                .await?
                .into_iter()
                .filter(|result| calls.contains(&result.call))
                .collect();
            let outcome =
                harnx_engine::tool::eval_tool_round(ctx, calls.clone(), completed, abort).await?;
            crate::fault_injection::hit(
                self.session.backend.session_id(),
                crate::fault_injection::RecoveryFaultPoint::RerunAfter,
            )
            .await;
            match outcome {
                ToolRoundOutcome::Completed(results) => return Ok(results),
                ToolRoundOutcome::InputRequired(suspension) => {
                    if !self.answer_live(ctx, &suspension).await? {
                        return Err(harnx_engine::tool::ToolInputRequiredError(suspension).into());
                    }
                }
            }
        }
    }

    pub async fn cleanup(&self) -> Result<()> {
        self.store
            .purge_prefix(&records::round_prefix(
                self.session.backend.session_id(),
                self.seq,
            ))
            .await
    }
}
