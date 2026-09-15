//! Restart discovery and generation-scoped cleanup updates share the commit gate.
use super::{index::Node, ledger, state::Member, *};
use crate::{CleanupState, CleanupStatus, ExecutionStore};
use anyhow::{ensure, Result};
use futures_util::TryStreamExt;

#[derive(Clone, Debug)]
pub struct CleanupScope {
    pub context: ExecutionContext,
    pub stop: StopReceipt,
    pub cleanup: CleanupStatus,
}

impl ExecutionStore {
    /// Startup/periodic discovery. A watch is only a wake-up; losing it cannot
    /// lose the stop, including a crash before its physical projection was made.
    pub async fn cleanup_scopes(&self) -> Result<Vec<CleanupScope>> {
        let mut keys = self.kv.keys().await?;
        let mut scopes = Vec::new();
        while let Some(key) = keys.try_next().await? {
            if !key.contains("/gates/") || !key.ends_with("/head") {
                continue;
            }
            let head: ledger::Head = super::index::read(&self.kv, &key).await?;
            scopes.extend(self.gate_cleanup_scopes(&head.root).await?);
        }
        Ok(scopes)
    }

    pub async fn gate_cleanup_scopes(
        &self,
        root: &crate::OperationRef,
    ) -> Result<Vec<CleanupScope>> {
        let snapshot = self.gate_snapshot(root).await?;
        let index = snapshot.index(self);
        let mut stack = index.root.clone().into_iter().collect::<Vec<_>>();
        let mut scopes = Vec::new();
        while let Some(node) = stack.pop() {
            match index.node(&node).await? {
                Node::Branch { zero, one, .. } => stack.extend([zero, one]),
                node => scopes.extend(scope_from_node(&index, root, node).await?),
            }
        }
        Ok(scopes)
    }

    /// Historical logical state, including after physical retirement. This is
    /// control evidence, never a work permit.
    pub async fn gate_logical_state(
        &self,
        context: &ExecutionContext,
    ) -> Result<crate::LogicalState> {
        let snapshot = self.gate_snapshot(context.gate_root()).await?;
        Ok(snapshot.index(self).check_identity(context).await?.logical)
    }

    pub async fn gate_cleanup(&self, context: &ExecutionContext) -> Result<CleanupStatus> {
        let snapshot = self.gate_snapshot(context.gate_root()).await?;
        Ok(snapshot.index(self).check_identity(context).await?.cleanup)
    }

    pub async fn record_cleanup(
        &self,
        context: &ExecutionContext,
        mut cleanup: CleanupStatus,
    ) -> Result<()> {
        let previous = self.gate_cleanup(context).await?;
        if previous.state == CleanupState::Confirmed {
            return Ok(());
        }
        cleanup.owner_stopped |= previous.owner_stopped;
        if previous.state == CleanupState::Unconfirmed && cleanup.state == CleanupState::Pending {
            cleanup.state = CleanupState::Unconfirmed;
            cleanup.last_error = cleanup.last_error.or(previous.last_error.clone());
        }
        if cleanup == previous {
            return Ok(());
        }
        self.commit_if_admissible(
            context,
            CommitAction {
                id: uuid::Uuid::now_v7().to_string(),
                kind: GateAction::CleanupUpdate { cleanup },
            },
        )
        .await?;
        Ok(())
    }

    /// Only an owner that has awaited its resource handles may call this.
    /// Descendant aggregation and retries belong to the background reconciler.
    pub async fn finish_cleanup_owner(&self, context: &ExecutionContext) -> Result<()> {
        let operation = self
            .owner_stopped(context.operation(), context.owner())
            .await?;
        self.record_cleanup(context, operation.cleanup_status())
            .await
    }

    /// An unresolved physical resource remains a durable blocker even if its
    /// local waiter returned. A later owner confirmation can still settle it.
    pub async fn unconfirm_cleanup_owner(
        &self,
        context: &ExecutionContext,
        reason: String,
    ) -> Result<()> {
        if let Some(operation) = self.get(context.operation()).await? {
            ensure!(
                operation.owner.as_ref() == Some(context.owner()),
                "cleanup owner changed"
            );
            self.mutate(context.operation(), |operation| {
                operation.check_owner(context.owner())?;
                operation.blocker = Some(reason.clone());
                if operation.state.cancelling()
                    && operation.state != crate::OperationState::Unconfirmed
                {
                    operation.transition(crate::OperationState::Unconfirmed)?;
                }
                Ok(())
            })
            .await?;
        }
        self.record_cleanup(context, CleanupStatus::unconfirmed(reason))
            .await
    }
}

async fn scope_from_node(
    index: &super::index::Index<'_>,
    root: &crate::OperationRef,
    node: Node,
) -> Result<Option<CleanupScope>> {
    match node {
        Node::Leaf { key, value } if key.starts_with("member/") => {
            scope_from_member(index, root, serde_json::from_value(value)?).await
        }
        _ => Ok(None),
    }
}

async fn scope_from_member(
    index: &super::index::Index<'_>,
    root: &crate::OperationRef,
    member: Member,
) -> Result<Option<CleanupScope>> {
    if member.cleanup.state == CleanupState::Confirmed {
        return Ok(None);
    }
    let Some(stop) = member.stop else {
        return Ok(None);
    };
    let generation = index.member(&member.generation).await?;
    Ok(Some(CleanupScope {
        context: ExecutionContext::new(
            member.generation,
            root.clone(),
            member.reference,
            (member.owner, generation.owner),
        ),
        stop,
        cleanup: member.cleanup,
    }))
}
