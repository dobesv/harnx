use super::{
    index::Index,
    state::{member_key, Member},
    types::*,
};
use crate::{CleanupState, LogicalState, OperationKind};
use anyhow::{ensure, Context, Result};

impl Index<'_> {
    pub async fn check_retry(&self, ctx: &ExecutionContext, action: &GateAction) -> Result<()> {
        // A historical positive commit is not a fresh work/consumption permit.
        // Recovery can query its proof separately even after the scope stopped.
        if matches!(
            action,
            GateAction::AdmitWork { .. }
                | GateAction::AdmitRecovery { .. }
                | GateAction::StartWork { .. }
                | GateAction::CommitOutput { .. }
                | GateAction::ConsumeReply { .. }
        ) {
            self.base_admissible(ctx).await?;
        }
        Ok(())
    }

    pub async fn check_repeated_action(
        &self,
        ctx: &ExecutionContext,
        action: &GateAction,
    ) -> Result<()> {
        match action {
            GateAction::AdmitRecovery { original } => self.recovery(ctx, original).await?,
            GateAction::StartWork { child, .. } => {
                self.base_admissible(&child.context(ctx)).await?;
            }
            GateAction::ConsumeReply { producer, reply } => {
                self.validate_reply(ctx, producer, reply).await?;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn apply(
        &mut self,
        ctx: &ExecutionContext,
        action: &GateAction,
        receipt: &CommitReceipt,
    ) -> Result<()> {
        match action {
            GateAction::AdmitWork { .. } => self.base_admissible(ctx).await.map(|_| ()),
            GateAction::AdmitRecovery { original } => self.recovery(ctx, original).await,
            GateAction::StartWork { child, .. } => self.start(ctx, child).await,
            GateAction::RegisterStoppedWork { child } => self.register_stopped(ctx, child).await,
            GateAction::CommitOutput { output } => self.output(ctx, output, receipt).await,
            GateAction::ConsumeReply { producer, reply } => {
                self.consume(ctx, (producer, reply), receipt).await
            }
            GateAction::CleanupUpdate { cleanup } => self.cleanup(ctx, cleanup.clone()).await,
            GateAction::ProjectCommitted {
                commit,
                projector,
                expected_cursor,
            } => {
                self.project(ctx, commit, (projector, expected_cursor.as_ref()))
                    .await
            }
            GateAction::RecordCancellation { fence_token, .. } => {
                self.cancellation_projection(ctx).await?;
                ensure!(
                    *fence_token >= ctx.owner().fence,
                    "cancel audit fence predates owner"
                );
                Ok(())
            }
            GateAction::FinishWork => self.finish(ctx).await,
            GateAction::ReplaceOwner { owner } => self.replace_owner(ctx, owner).await,
            GateAction::ReplaceGeneration { generation, owner } => {
                self.replace_generation(ctx, generation, owner).await
            }
        }
    }

    async fn recovery(&self, ctx: &ExecutionContext, original: &ExecutionContext) -> Result<()> {
        original.validate()?;
        ensure!(original.gate_root == ctx.gate_root, "recovery gate changed");
        // Stop on the original work dominates even a G2 adoption attempt.
        if let Some(stop) = self.stop_for(original.operation()).await? {
            return Err(Interrupted { stop }.into());
        }
        let source = self.member(original.operation()).await?;
        ensure!(
            source.generation == original.generation,
            "recovery identity changed"
        );
        ensure!(
            original.generation == ctx.generation,
            "recovery cannot adopt another generation"
        );
        self.base_admissible(ctx).await?;
        Ok(())
    }

    async fn output(
        &mut self,
        ctx: &ExecutionContext,
        output: &CommittedOutput,
        receipt: &CommitReceipt,
    ) -> Result<()> {
        let member = self.base_admissible(ctx).await?;
        validate_id(&output.id)?;
        match output.kind {
            OutputKind::ToolReply => ensure!(
                member.kind == OperationKind::Tool,
                "reply requires a tool operation"
            ),
            OutputKind::ModelResponse | OutputKind::SessionMetadata => ensure!(
                member.kind == OperationKind::Session,
                "output requires a session operation"
            ),
            OutputKind::Transcript | OutputKind::Progress | OutputKind::Lifecycle => {}
        }
        let key = output_key(ctx, output)?;
        ensure!(
            self.get::<CommitReceipt>(&key).await?.is_none(),
            "output already committed"
        );
        self.set(&key, receipt).await
    }

    async fn consume(
        &mut self,
        ctx: &ExecutionContext,
        source: (&ExecutionContext, &CommitReceipt),
        receipt: &CommitReceipt,
    ) -> Result<()> {
        let (producer, reply) = source;
        self.base_admissible(ctx).await?;
        self.validate_reply(ctx, producer, reply).await?;
        let key = format!(
            "consumed/{}/{}/{}/{}",
            ctx.operation.key(),
            ctx.owner.instance_id,
            ctx.owner.fence,
            producer.operation.key()
        );
        ensure!(
            self.get::<CommitReceipt>(&key).await?.is_none(),
            "reply already consumed"
        );
        self.set(&key, receipt).await
    }

    async fn validate_reply(
        &self,
        ctx: &ExecutionContext,
        producer: &ExecutionContext,
        reply: &CommitReceipt,
    ) -> Result<()> {
        producer.validate()?;
        ensure!(
            producer.gate_root == ctx.gate_root,
            "reply belongs to another gate"
        );
        let source = self.member(&producer.operation).await?;
        ensure!(
            source.parent.as_ref() == Some(&ctx.operation),
            "reply producer is not this consumer's child"
        );
        if let Some(stop) = self.stop_for(&producer.operation).await? {
            return Err(Interrupted { stop }.into());
        }
        let committed = self.decision(reply).await?;
        ensure!(
            committed.context == *producer,
            "reply producer identity mismatch"
        );
        ensure!(
            matches!(
                committed.action,
                CommittedAction::Action {
                    action: CommitAction {
                        kind: GateAction::CommitOutput {
                            output: CommittedOutput {
                                kind: OutputKind::ToolReply,
                                ..
                            }
                        },
                        ..
                    }
                }
            ),
            "commit is not a tool reply"
        );
        Ok(())
    }

    async fn cleanup(
        &mut self,
        ctx: &ExecutionContext,
        mut cleanup: crate::CleanupStatus,
    ) -> Result<()> {
        let mut member = self.check_identity(ctx).await?;
        ensure!(
            member.cleanup.state != CleanupState::Confirmed
                || cleanup.state == CleanupState::Confirmed,
            "confirmed cleanup cannot reopen"
        );
        cleanup.owner_stopped |= member.cleanup.owner_stopped;
        if member.cleanup.state == CleanupState::Unconfirmed
            && cleanup.state == CleanupState::Pending
        {
            cleanup.state = CleanupState::Unconfirmed;
        }
        ensure!(
            cleanup.state != CleanupState::Confirmed
                || (cleanup.owner_stopped && cleanup.remaining == 0),
            "cleanup confirmation requires owner and descendant evidence"
        );
        member.cleanup = cleanup;
        self.save_member(&member).await
    }

    async fn project(
        &mut self,
        ctx: &ExecutionContext,
        commit: &CommitReceipt,
        cursor: (&str, Option<&CommitReceipt>),
    ) -> Result<()> {
        let (projector, expected) = cursor;
        validate_id(projector)?;
        let decision = self.decision(commit).await?;
        ensure!(
            decision.context == *ctx,
            "projection context does not match committed payload"
        );
        let key = format!("projector/{projector}");
        let cursor: Option<CommitReceipt> = self.get(&key).await?;
        ensure!(cursor.as_ref() == expected, "projection cursor changed");
        ensure!(
            cursor
                .as_ref()
                .is_none_or(|cursor| cursor.sequence < commit.sequence),
            "projection cursor cannot regress"
        );
        self.set(&key, commit).await
    }

    async fn cancellation_projection(&self, ctx: &ExecutionContext) -> Result<()> {
        let member = self.check_identity(ctx).await?;
        ensure!(
            member.kind == OperationKind::Session,
            "cancel projection requires a session"
        );
        ensure!(
            self.stop_for(ctx.operation()).await?.is_some(),
            "cancel projection requires a stop"
        );
        let current: crate::OperationRef = self
            .get(&super::state::generation_key(&ctx.generation.session_id))
            .await?
            .context("gate current generation missing")?;
        ensure!(
            current == ctx.generation,
            "cancel projection generation replaced"
        );
        Ok(())
    }

    async fn finish(&mut self, ctx: &ExecutionContext) -> Result<()> {
        let mut member = self.base_admissible(ctx).await?;
        member.logical = LogicalState::Completed;
        self.save_member(&member).await
    }

    async fn replace_owner(&mut self, ctx: &ExecutionContext, owner: &crate::Owner) -> Result<()> {
        validate_owner(owner)?;
        let mut member = self.owner_handover_member(ctx).await?;
        ensure!(
            owner.fence > member.owner.fence,
            "replacement owner fence must advance"
        );
        member.owner = owner.clone();
        self.save_member(&member).await
    }

    async fn owner_handover_member(&self, ctx: &ExecutionContext) -> Result<Member> {
        if self.member(ctx.operation()).await?.kind != OperationKind::Session
            || self.stop_for(ctx.operation()).await?.is_none()
        {
            return self.base_admissible(ctx).await;
        }
        // A replacement session worker must project Cancel/cleanup with its own
        // lease fence. Changing owner never clears stop or grants output authority.
        self.cancellation_projection(ctx).await?;
        self.check_identity(ctx).await
    }

    async fn replace_generation(
        &mut self,
        ctx: &ExecutionContext,
        generation: &crate::OperationRef,
        owner: &crate::Owner,
    ) -> Result<()> {
        validate_reference(generation)?;
        validate_owner(owner)?;
        let previous = self.check_identity(ctx).await?;
        ensure!(
            previous.parent.is_none() && previous.kind == OperationKind::Session,
            "only a gate root generation can be replaced here"
        );
        ensure!(
            generation.session_id == ctx.generation.session_id,
            "replacement changed session identity"
        );
        ensure!(
            self.get::<Member>(&member_key(generation)).await?.is_none(),
            "generation identity cannot be reused"
        );
        let current: crate::OperationRef = self
            .get(&super::state::generation_key(&generation.session_id))
            .await?
            .context("gate generation missing")?;
        ensure!(current == previous.reference, "generation already replaced");
        let member = Member::root(generation.clone(), owner.clone());
        self.install_generation(&member).await?;
        self.save_member(&member).await
    }
}

pub(super) fn output_key(ctx: &ExecutionContext, output: &CommittedOutput) -> Result<String> {
    let slot = if output.kind == OutputKind::ToolReply {
        "reply"
    } else {
        &output.id
    };
    Ok(format!(
        "output/{}/{}/{}",
        ctx.operation.key(),
        serde_json::to_string(&output.kind)?,
        slot
    ))
}
