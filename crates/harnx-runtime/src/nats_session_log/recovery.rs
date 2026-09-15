//! Resolve durable ownership before reconstructing model history. Neither a
//! transcript fence nor a journal hit is permission to start work.
use super::*;
use crate::execution_fence::GenerationFence;
use harnx_execution_control::{
    CommitAction, CommittedAction, ExecutionContext, ExecutionStore, GateAction, Interrupted,
    OperationRef, OutputKind, RecoveryHistory,
};

pub(crate) fn prompt_owner<'a>(
    history: &'a [RecoveryHistory],
    id: Option<&str>,
    seq: u64,
) -> Result<Option<&'a RecoveryHistory>> {
    let mut found = None;
    for record in history {
        let by_id = id.and_then(|id| record.admissions.get(id));
        if by_id.is_none() && !record.admissions.values().any(|value| *value == Some(seq)) {
            continue;
        }
        anyhow::ensure!(
            by_id.is_none_or(|value| value.is_none_or(|value| value == seq)),
            "prompt admission sequence changed"
        );
        anyhow::ensure!(found.is_none(), "ambiguous prompt generation history");
        found = Some(record);
    }
    Ok(found)
}

impl NatsSessionLog {
    /// Exact Stage 4 transcript proof, or an unambiguous retained legacy worker
    /// fence. Never bind a call to whichever generation happens to be current.
    pub(crate) async fn entry_authority(
        &self,
        store: &ExecutionStore,
        seq: u64,
        expected: &[harnx_core::tool::ToolCall],
    ) -> Result<ExecutionContext> {
        let raw = self.ensure_stream().await?.get_raw_message(seq).await?;
        let entry = deserialize_entry(&raw.payload)?;
        let SessionLogEntry::ToolCalls { calls, .. } = &entry else {
            anyhow::bail!("recovery source is not a tool round");
        };
        anyhow::ensure!(
            serde_json::to_value(calls)? == serde_json::to_value(expected)?,
            "modified tool round has unknown recovery authority"
        );
        let root = store
            .gate_root(&self.session_id)
            .await?
            .context("unknown recovery gate")?;
        if let Some(id) = raw
            .headers
            .get(async_nats::header::NATS_MESSAGE_ID)
            .and_then(|id| id.as_str().strip_prefix("gate-"))
        {
            let decision = store.gate_decision_by_id(&root, id).await?;
            anyhow::ensure!(
                matches!(decision.action, CommittedAction::Action {
                action: CommitAction { kind: GateAction::CommitOutput { ref output }, .. }
            } if output.kind == OutputKind::Transcript),
                "recovery entry has no transcript proof"
            );
            let payload = store.committed_output_payload(&decision.receipt).await?;
            anyhow::ensure!(
                payload["entry"] == serde_json::to_value(&entry)?,
                "recovery transcript proof mismatch"
            );
            anyhow::ensure!(
                decision.context.generation().session_id == self.session_id,
                "recovery transcript session mismatch"
            );
            return Ok(decision.context);
        }
        if entry.fence_token().is_none() {
            return self.journal_authority(seq, &entry).await;
        }
        let fence = entry
            .fence_token()
            .context("unknown legacy tool-call generation")?;
        let history = store.recovery_history(&self.session_id).await?;
        let mut owners = history.iter().filter(|record| {
            record.kind == Some(harnx_execution_control::OperationKind::Session)
                && record.owner_fences.contains(&fence)
        });
        let owner = owners.next().context("unknown legacy worker fence")?;
        anyhow::ensure!(owners.next().is_none(), "ambiguous legacy worker fence");
        let context = store.gate_context(&root, &owner.reference).await?;
        if let Some(stop) = store.gate_stop(&root, &owner.reference).await? {
            return Err(Interrupted { stop }.into());
        }
        // A first claim cannot lend its freshly acquired fence to an older,
        // unbound transcript. Only a predecessor's fence is legacy evidence.
        anyhow::ensure!(
            fence < context.owner().fence,
            "unknown legacy predecessor fence"
        );
        Ok(context)
    }

    async fn journal_authority(
        &self,
        seq: u64,
        entry: &SessionLogEntry,
    ) -> Result<ExecutionContext> {
        let SessionLogEntry::ToolCalls { calls, .. } = entry else {
            anyhow::bail!("unknown legacy tool-call generation");
        };
        let journal =
            harnx_toolset_server::invocation_journal::InvocationJournal::ensure(&self.jetstream)
                .await?;
        let mut original: Option<ExecutionContext> = None;
        for call in calls {
            let id = call
                .id
                .as_deref()
                .context("unknown legacy tool-call generation")?;
            let record = journal
                .find(&self.session_id, seq, id)
                .await?
                .context("unknown legacy tool-call generation")?;
            anyhow::ensure!(
                record.tool_name == call.name,
                "recovery tool identity changed"
            );
            let context = record
                .request
                .execution
                .context("unknown journal generation")?
                .consumer;
            anyhow::ensure!(
                original
                    .as_ref()
                    .is_none_or(|old| old.generation() == context.generation()
                        && old.gate_root() == context.gate_root()),
                "ambiguous tool-round generation"
            );
            original = Some(context);
        }
        original.context("unknown legacy tool-call generation")
    }

    /// Reconcile a committed stop before model reconstruction or owner claim.
    /// Old control may not cover new-generation prompts. Prompt admission
    /// projects this boundary before installing G2, independently of cleanup.
    pub(crate) async fn recover_stop(
        &self,
        store: &ExecutionStore,
        reference: &OperationRef,
    ) -> Result<Option<Interrupted>> {
        if store
            .get(reference)
            .await?
            .is_some_and(|operation| operation.gate_registration.is_some())
        {
            // The immutable marker can survive a crash before the gate's first CAS.
            // Help finish that exact registration, never import unbound history.
            store.activate_gate(reference).await?;
        }
        let Some(root) = store.gate_root(&self.session_id).await? else {
            return Ok(None);
        };
        let Some(stop) = store.gate_stop(&root, reference).await? else {
            return Ok(None);
        };
        // Verifies retained lineage, including descendants pruned before wakeup.
        store.committed_decision(&stop.commit).await?;
        let context = store.gate_context(&root, reference).await?;
        let fence = GenerationFence::new(store.clone(), context);
        if store.gate_generation(&root, &self.session_id).await? == *reference {
            self.project_through(&fence, &stop.commit).await?;
            self.project_recovered_cancellation(&fence).await?;
        }
        Ok(Some(Interrupted { stop }))
    }

    async fn project_recovered_cancellation(&self, fence: &GenerationFence) -> Result<()> {
        for _ in 0..32 {
            if fence
                .store
                .gate_generation(fence.context.gate_root(), &self.session_id)
                .await?
                != *fence.context.generation()
            {
                return Ok(());
            }
            let entries = self.load_events_latest_async().await?;
            if matches!(entries.last(), Some((_, SessionLogEntry::Cancel { .. }))) {
                return Ok(());
            }
            if self
                .append_cancellation(
                    fence,
                    (
                        entries.last().map_or(0, |(seq, _)| *seq),
                        fence.context.owner().fence,
                    ),
                )
                .await?
                .is_some()
            {
                return Ok(());
            }
            // A concurrent worker/recovery projector changed the tail. Reload
            // its durable boundary instead of failing G2 admission on a lost CAS.
        }
        anyhow::bail!("cancellation projection remained busy; retry admission")
    }

    pub(super) async fn validate_cancel_ownership(
        &self,
        store: &ExecutionStore,
        reference: &OperationRef,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<()> {
        let history = store.recovery_history(&self.session_id).await?;
        for (seq, id) in pending_prompts(entries)? {
            let owner = prompt_owner(&history, id.as_deref(), seq)?
                .context("unknown cancellation prompt history")?;
            anyhow::ensure!(
                owner.reference == *reference,
                "cannot project old cancellation over another generation"
            );
        }
        Ok(())
    }

    pub(crate) async fn admit_reconstruction(&self, fence: &GenerationFence) -> Result<()> {
        if let Some(interrupted) = self
            .recover_stop(&fence.store, fence.context.generation())
            .await?
        {
            return Err(interrupted.into());
        }
        // Historical projection is not work admission. Resolve original prompt
        // ownership before admitting a reconstruction under the claimed receiver.
        let receipt = fence.store.gate_tip(fence.context.gate_root()).await?;
        self.project_through(fence, &receipt).await?;
        self.validate_pending_ownership(fence).await?;
        fence.check("recovery-bootstrap").await
    }

    /// Every unanswered prompt must have its original reservation. Unbound old
    /// transcripts require explicit repair, never a fresh automatic admission.
    async fn validate_pending_ownership(&self, fence: &GenerationFence) -> Result<()> {
        let entries = self.load_events_latest_async().await?;
        let history = fence.store.recovery_history(&self.session_id).await?;
        for (seq, id) in pending_prompts(&entries)? {
            let owner = prompt_owner(&history, id.as_deref(), seq)?
                .with_context(|| format!("unknown pending prompt generation; automatic adoption refused: session={} seq={seq}", self.session_id))?;
            let original = fence
                .store
                .gate_context(fence.context.gate_root(), &owner.reference)
                .await?;
            admit_original(fence, original).await?;
            if let Some(id) = id
                .as_deref()
                .filter(|id| owner.admissions.get(*id) == Some(&None))
            {
                fence.store.commit_prompt(&owner.reference, id, seq).await?;
            }
        }
        Ok(())
    }
}

pub(crate) async fn admit_original(
    fence: &GenerationFence,
    original: ExecutionContext,
) -> Result<()> {
    fence
        .store
        .commit_if_admissible(
            &fence.context,
            CommitAction {
                id: uuid::Uuid::now_v7().to_string(),
                kind: GateAction::AdmitRecovery { original },
            },
        )
        .await?;
    Ok(())
}

fn pending_prompts(entries: &[(u64, SessionLogEntry)]) -> Result<Vec<(u64, Option<String>)>> {
    let mut pending = Vec::new();
    for (seq, entry) in harnx_core::session_reconstruct::apply_log_mutations_nats(entries)? {
        if let SessionLogEntry::Message { id, role, .. } = entry {
            if role.is_user()
                && crate::nats_session::requested_seq_status(entries, seq)?
                    == crate::nats_session::RequestedSeqStatus::Pending
            {
                pending.push((seq, id));
            }
        }
    }
    Ok(pending)
}
