//! Opt-in physical graph bridge for tool reply fencing. The registration marker
//! CAS arbitrates activation versus legacy cancellation; after activation only
//! the tree gate authorizes work. Physical graph writes remain cleanup bookkeeping.
use super::{state::Member, types::*};
use crate::{ExecutionStore, Operation, OperationKind, OperationRef, Owner};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRegistration {
    pub context: ExecutionContext,
    pub parent: Option<ExecutionContext>,
    pub kind: OperationKind,
    pub previous_generation: Option<OperationRef>,
}

impl ExecutionStore {
    /// Capture authority before creating an invocation. Never call this to adopt
    /// a legacy saved reply: recovery must use its original recorded context.
    pub async fn activate_gate(&self, reference: &OperationRef) -> Result<ExecutionContext> {
        let mut path = Vec::new();
        let mut next = Some(reference.clone());
        let mut seen = std::collections::BTreeSet::new();
        let mut parent = None;
        while let Some(reference) = next {
            ensure!(
                seen.insert(reference.clone()),
                "gate activation lineage cycle"
            );
            if let Some(registration) = self.recovery_registration(&reference).await? {
                Box::pin(self.materialize_gate(&registration)).await?;
                parent = Some(
                    self.gate_context(registration.context.gate_root(), &reference)
                        .await?,
                );
                break;
            }
            let operation = self
                .get(&reference)
                .await?
                .context("gate activation node missing")?;
            next = operation.parent.clone();
            path.push(operation);
        }
        for operation in path.into_iter().rev() {
            parent = Some(Box::pin(self.activate_node(operation, parent)).await?);
        }
        parent.context("gate activation identity missing")
    }

    async fn activate_node(
        &self,
        operation: Operation,
        parent: Option<ExecutionContext>,
    ) -> Result<ExecutionContext> {
        let owner = match &operation.owner {
            Some(owner) => owner.clone(),
            None => {
                ensure!(
                    operation.kind == OperationKind::Tool,
                    "gate session owner missing"
                );
                Owner::invocation("tool-dispatch")
            }
        };
        let registration = self.registration(&operation, parent, owner).await?;
        let marked = self
            .mutate(&operation.reference, |op| {
                if op.gate_registration.is_none() {
                    ensure!(
                        op.allows_continuation(),
                        "cannot activate a cancelled generation"
                    );
                    ensure!(
                        op.owner == operation.owner && op.parent == operation.parent,
                        "activation owner changed"
                    );
                    op.gate_registration = Some(Box::new(registration.clone()));
                }
                Ok(())
            })
            .await?;
        let registration = marked
            .gate_registration
            .context("gate activation marker missing")?;
        Box::pin(self.materialize_gate(&registration)).await?;
        self.gate_context(registration.context.gate_root(), &operation.reference)
            .await
    }

    async fn registration(
        &self,
        operation: &Operation,
        parent: Option<ExecutionContext>,
        owner: Owner,
    ) -> Result<GateRegistration> {
        let context = if let Some(parent) = &parent {
            WorkRegistration {
                operation: operation.reference.clone(),
                kind: operation.kind,
                owner,
            }
            .context(parent)
        } else {
            ensure!(
                operation.kind == OperationKind::Session,
                "standalone tool needs a receiving generation"
            );
            let root = self
                .gate_root(&operation.reference.session_id)
                .await?
                .unwrap_or_else(|| operation.reference.clone());
            ExecutionContext::new(
                operation.reference.clone(),
                root,
                operation.reference.clone(),
                (owner.clone(), owner),
            )
        };
        Ok(GateRegistration {
            context,
            parent,
            kind: operation.kind,
            previous_generation: operation.previous_generation.clone(),
        })
    }

    pub(crate) async fn materialize_gate(&self, registration: &GateRegistration) -> Result<()> {
        let ctx = &registration.context;
        if let Some(parent) = &registration.parent {
            self.materialize_child(registration, parent).await?;
        } else if ctx.operation == ctx.gate_root {
            let future = self.open_gate(ctx.gate_root.clone(), ctx.owner.clone());
            Box::pin(future).await?;
        } else {
            self.install_bridge_generation(registration).await?;
        }
        Ok(())
    }

    async fn materialize_child(
        &self,
        registration: &GateRegistration,
        parent: &ExecutionContext,
    ) -> Result<()> {
        let ctx = &registration.context;
        let action = CommitAction {
            id: format!("register-{}", ctx.operation.execution_id),
            kind: GateAction::StartWork {
                child: WorkRegistration {
                    operation: ctx.operation.clone(),
                    kind: registration.kind,
                    owner: ctx.owner.clone(),
                },
                input: serde_json::Value::Null,
            },
        };
        // A parent handover can race the child's activation marker. The marker
        // fixes lineage/generation, not a forever-current parent owner. First
        // recover an already committed registration, regardless of writer owner.
        if self.bridge_child_registered(registration, parent).await? {
            return Ok(());
        }
        let current_parent = self
            .gate_context(parent.gate_root(), parent.operation())
            .await?;
        ensure!(
            current_parent.generation() == parent.generation(),
            "bridge registration generation changed"
        );
        // Every initialization still races stop through StartWork's gate CAS.
        if self
            .committed_action(&current_parent, &action)
            .await?
            .is_none()
        {
            match self.commit_if_admissible(&current_parent, action).await {
                Err(error)
                    if error.is::<Interrupted>() && registration.kind == OperationKind::Session =>
                {
                    self.register_stopped_child(ctx, &current_parent).await?;
                }
                result => {
                    result?;
                }
            }
        }
        Ok(())
    }

    async fn bridge_child_registered(
        &self,
        registration: &GateRegistration,
        parent: &ExecutionContext,
    ) -> Result<bool> {
        let ctx = &registration.context;
        let snapshot = self.gate_snapshot(ctx.gate_root()).await?;
        let Some(member) = snapshot
            .index(self)
            .get::<Member>(&super::state::member_key(ctx.operation()))
            .await?
        else {
            return Ok(false);
        };
        ensure!(
            member.reference == *ctx.operation() && member.generation == *ctx.generation(),
            "bridge child identity changed"
        );
        ensure!(
            member.parent.as_ref() == Some(parent.operation()) && member.kind == registration.kind,
            "bridge child lineage changed"
        );
        Ok(true)
    }

    async fn register_stopped_child(
        &self,
        ctx: &ExecutionContext,
        parent: &ExecutionContext,
    ) -> Result<()> {
        // Parent acceptance can precede the child's first claim.
        // Recover its original lineage for control projection only.
        self.commit_if_admissible(
            parent,
            CommitAction {
                id: format!("stopped-register-{}", ctx.operation.execution_id),
                kind: GateAction::RegisterStoppedWork {
                    child: WorkRegistration {
                        operation: ctx.operation.clone(),
                        kind: OperationKind::Session,
                        owner: ctx.owner.clone(),
                    },
                },
            },
        )
        .await?;
        Ok(())
    }

    async fn install_bridge_generation(&self, registration: &GateRegistration) -> Result<()> {
        let ctx = &registration.context;
        let snapshot = self.gate_snapshot(&ctx.gate_root).await?;
        let index = snapshot.index(self);
        if index
            .get::<Member>(&super::state::member_key(&ctx.operation))
            .await?
            .is_some()
        {
            return Ok(());
        }
        let previous = registration
            .previous_generation
            .as_ref()
            .context("previous generation authority missing")?;
        let old = self.gate_context(&ctx.gate_root, previous).await?;
        self.commit_if_admissible(
            &old,
            CommitAction {
                id: format!("generation-{}", ctx.generation.execution_id),
                kind: GateAction::ReplaceGeneration {
                    generation: ctx.generation.clone(),
                    owner: ctx.owner.clone(),
                },
            },
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn can_replace_session_generation(
        &self,
        operation: &Operation,
    ) -> Result<bool> {
        if operation.can_replace_generation() {
            return Ok(true);
        }
        let Some(registration) = &operation.gate_registration else {
            return Ok(false);
        };
        // A retained stop is monotonic negative evidence, even when its physical
        // projection/cleanup was lost. Installing work still requires gate CAS.
        Ok(self
            .gate_stop(registration.context.gate_root(), &operation.reference)
            .await?
            .is_some())
    }

    /// Read retained interruption for this exact generation without helping
    /// projection or cleanup. An unfinished registration is still pending.
    pub async fn accepted_stop(&self, reference: &OperationRef) -> Result<Option<StopReceipt>> {
        let Some(registration) = self.recovery_registration(reference).await? else {
            return Ok(None);
        };
        self.gate_stop_if_registered(registration.context.gate_root(), reference)
            .await
    }

    /// Snapshot for creation/recovery adapters, NOT an execution permit.
    pub async fn gate_context(
        &self,
        root: &OperationRef,
        reference: &OperationRef,
    ) -> Result<ExecutionContext> {
        let snapshot = self.gate_snapshot(root).await?;
        let index = snapshot.index(self);
        let member = index.member(reference).await?;
        let generation = index.member(&member.generation).await?;
        Ok(ExecutionContext::new(
            member.generation,
            root.clone(),
            reference.clone(),
            (member.owner, generation.owner),
        ))
    }

    pub async fn gate_root(&self, session: &str) -> Result<Option<OperationRef>> {
        self.kv
            .get(format!("sessions/{session}/gate-authority"))
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    pub(crate) async fn bridge_owner(&self, op: &Operation, owner: &Owner) -> Result<()> {
        // Tool attempts transfer their gate reservation before claiming physical
        // ownership. A physical claim alone must not mint logical authority.
        if op.kind == OperationKind::Tool {
            return Ok(());
        }
        let reference = &op.reference;
        let registration = match &op.gate_registration {
            Some(registration) => registration.clone(),
            None if op.kind == OperationKind::Session
                && self.gate_root(&reference.session_id).await?.is_some() =>
            {
                self.activate_gate(reference).await?;
                return Ok(());
            }
            None => return Ok(()),
        };
        self.materialize_gate(&registration).await?;
        let ctx = self
            .gate_context(registration.context.gate_root(), reference)
            .await?;
        if ctx.owner() != owner {
            self.commit_if_admissible(
                &ctx,
                CommitAction {
                    id: format!("owner-{}-{}", owner.instance_id, owner.fence),
                    kind: GateAction::ReplaceOwner {
                        owner: owner.clone(),
                    },
                },
            )
            .await?;
        }
        Ok(())
    }

    pub(crate) async fn bridge_finish(&self, operation: &Operation) -> Result<()> {
        let Some(ctx) = self.completed_bridge_context(operation).await? else {
            return Ok(());
        };
        let action = CommitAction {
            id: format!("finish-{}-{}", ctx.owner.instance_id, ctx.owner.fence),
            kind: GateAction::FinishWork,
        };
        if self.committed_action(&ctx, &action).await?.is_some() {
            return Ok(());
        }
        match self.commit_if_admissible(&ctx, action).await {
            Ok(_) => Ok(()),
            Err(error) if error.is::<Interrupted>() => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn completed_bridge_context(
        &self,
        operation: &Operation,
    ) -> Result<Option<ExecutionContext>> {
        if operation.state != crate::OperationState::Completed {
            return Ok(None);
        }
        let Some(registration) = &operation.gate_registration else {
            return Ok(None);
        };
        self.materialize_gate(registration).await?;
        let ctx = self
            .gate_context(registration.context.gate_root(), &operation.reference)
            .await?;
        if operation.owner.as_ref() != Some(ctx.owner())
            || self
                .gate_stop(ctx.gate_root(), ctx.operation())
                .await?
                .is_some()
        {
            return Ok(None);
        }
        if operation.kind == OperationKind::Tool && self.committed_tool_reply(&ctx).await?.is_none()
        {
            // Rejected old-owner output isn't logical completion. Replay may
            // still produce this invocation's first committed reply.
            return Ok(None);
        }
        Ok(Some(ctx))
    }

    /// A no-marker result is safe only from the same physical CAS that records
    /// cancellation, not a preceding GET. It permanently prevents activation.
    pub(crate) async fn bridge_cancel(&self, operation: &Operation, id: &str) -> Result<()> {
        if operation.state == crate::OperationState::Completed {
            return self.bridge_finish(operation).await;
        }
        let Some(registration) = &operation.gate_registration else {
            return Ok(());
        };
        if let Err(error) = self.materialize_gate(registration).await {
            if error.is::<Interrupted>() {
                return Ok(());
            }
            return Err(error);
        }
        if self
            .gate_stop(registration.context.gate_root(), &operation.reference)
            .await?
            .is_some()
        {
            return Ok(());
        }
        self.interrupt(
            &InterruptScope {
                gate_root: registration.context.gate_root.clone(),
                operation: operation.reference.clone(),
                reason: "cancellation requested".into(),
            },
            id,
        )
        .await?;
        Ok(())
    }
}
