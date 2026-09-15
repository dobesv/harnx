use super::{index::Index, types::*};
use crate::{LogicalState, OperationKind, OperationRef, Owner};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Member {
    pub reference: OperationRef,
    pub parent: Option<OperationRef>,
    pub generation: OperationRef,
    pub kind: OperationKind,
    pub owner: Owner,
    pub logical: LogicalState,
    pub cleanup: crate::CleanupStatus,
    pub stop: Option<StopReceipt>,
}

impl Member {
    pub fn root(reference: OperationRef, owner: Owner) -> Self {
        Self {
            generation: reference.clone(),
            reference,
            parent: None,
            kind: OperationKind::Session,
            owner,
            logical: LogicalState::Running,
            cleanup: crate::CleanupStatus::default(),
            stop: None,
        }
    }
}

impl Index<'_> {
    pub async fn member(&self, reference: &OperationRef) -> Result<Member> {
        validate_reference(reference)?;
        let member: Member = self
            .get(&member_key(reference))
            .await?
            .context("operation not registered in gate")?;
        ensure!(
            member.reference == *reference,
            "gate registration identity mismatch"
        );
        Ok(member)
    }

    pub async fn save_member(&mut self, member: &Member) -> Result<()> {
        self.set(&member_key(&member.reference), member).await
    }

    pub async fn check_identity(&self, ctx: &ExecutionContext) -> Result<Member> {
        ctx.validate()?;
        let op = self.member(&ctx.operation).await?;
        ensure!(
            op.generation == ctx.generation,
            "gate generation identity mismatch"
        );
        ensure!(op.owner == ctx.owner, "gate owner fence changed");
        let generation = self.member(&ctx.generation).await?;
        ensure!(
            generation.kind == OperationKind::Session,
            "gate generation is not a session"
        );
        Ok(op)
    }

    pub async fn base_admissible(&self, ctx: &ExecutionContext) -> Result<Member> {
        // Stop dominates stale owner/generation errors, including late replay
        // attempts. This read is from the snapshot validated by the same CAS.
        if let Some(stop) = self.stop_for(&ctx.operation).await? {
            return Err(Interrupted { stop }.into());
        }
        let op = self.check_identity(ctx).await?;
        let generation = self.member(&ctx.generation).await?;
        ensure!(
            generation.owner == ctx.generation_owner,
            "gate generation owner fence changed"
        );
        self.check_lineage(&ctx.operation).await?;
        Ok(op)
    }

    /// Walk parent links only. A scope stop does not update or enumerate children.
    pub async fn check_lineage(&self, reference: &OperationRef) -> Result<()> {
        let mut next = Some(reference.clone());
        let mut seen = BTreeSet::new();
        while let Some(reference) = next {
            ensure!(seen.insert(reference.clone()), "gate lineage cycle");
            let op = self.member(&reference).await?;
            if let Some(stop) = op.stop {
                return Err(Interrupted { stop }.into());
            }
            ensure!(op.logical.accepts_work(), "gate generation is stopped");
            let current: OperationRef = self
                .get(&generation_key(&op.generation.session_id))
                .await?
                .context("gate current generation missing")?;
            ensure!(current == op.generation, "gate generation replaced");
            next = op.parent;
        }
        Ok(())
    }

    pub async fn stop_for(&self, reference: &OperationRef) -> Result<Option<StopReceipt>> {
        let mut next = Some(reference.clone());
        let mut seen = BTreeSet::new();
        while let Some(reference) = next {
            ensure!(seen.insert(reference.clone()), "gate lineage cycle");
            let member = self.member(&reference).await?;
            if let Some(stop) = member.stop {
                return Ok(Some(stop));
            }
            next = member.parent;
        }
        Ok(None)
    }

    pub async fn start(&mut self, ctx: &ExecutionContext, child: &WorkRegistration) -> Result<()> {
        self.base_admissible(ctx).await?;
        validate_reference(&child.operation)?;
        validate_owner(&child.owner)?;
        ensure!(
            self.get::<Member>(&member_key(&child.operation))
                .await?
                .is_none(),
            "gate operation already registered"
        );
        if child.kind == OperationKind::Tool {
            ensure!(
                child.operation.session_id == ctx.generation.session_id,
                "tool generation session mismatch"
            );
        }
        let identity = child.context(ctx);
        let member = Member {
            reference: child.operation.clone(),
            parent: Some(ctx.operation.clone()),
            generation: identity.generation,
            kind: child.kind,
            owner: child.owner.clone(),
            logical: LogicalState::Running,
            cleanup: crate::CleanupStatus::default(),
            stop: None,
        };
        if child.kind == OperationKind::Session {
            self.install_generation(&member).await?;
        }
        self.save_member(&member).await
    }

    pub async fn register_stopped(
        &mut self,
        ctx: &ExecutionContext,
        child: &WorkRegistration,
    ) -> Result<()> {
        self.check_identity(ctx).await?;
        let stop = self
            .stop_for(ctx.operation())
            .await?
            .context("stopped registration requires parent interruption")?;
        validate_reference(&child.operation)?;
        validate_owner(&child.owner)?;
        ensure!(
            child.kind == OperationKind::Session,
            "stopped registration requires a session"
        );
        ensure!(
            self.get::<Member>(&member_key(&child.operation))
                .await?
                .is_none(),
            "gate operation already registered"
        );
        let identity = child.context(ctx);
        let member = Member {
            reference: child.operation.clone(),
            parent: Some(ctx.operation.clone()),
            generation: identity.generation,
            kind: child.kind,
            owner: child.owner.clone(),
            logical: LogicalState::Interrupted,
            cleanup: crate::CleanupStatus::default(),
            stop: Some(stop),
        };
        self.install_generation(&member).await?;
        self.save_member(&member).await
    }

    pub async fn install_generation(&mut self, member: &Member) -> Result<()> {
        let key = generation_key(&member.reference.session_id);
        if let Some(previous) = self.get::<OperationRef>(&key).await? {
            ensure!(
                self.member(&previous)
                    .await?
                    .logical
                    .can_replace_generation()
                    || self.stop_for(&previous).await?.is_some(),
                "gate generation still active"
            );
        }
        self.set(&key, &member.reference).await
    }
}

pub(super) fn member_key(reference: &OperationRef) -> String {
    format!("member/{}", reference.key())
}
pub(super) fn generation_key(session: &str) -> String {
    format!("generation/{session}")
}
pub(super) fn action_key(ctx: &ExecutionContext, id: &str) -> String {
    format!("action/{}/{}", ctx.operation.key(), id)
}
