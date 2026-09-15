use super::{
    index,
    ledger::{anchor_key, Head, Snapshot},
    state::{action_key, Member},
    types::*,
};
use crate::{ExecutionStore, LogicalState, OperationRef, Owner, StopDecision};
use anyhow::{ensure, Context, Result};
use chrono::Utc;

pub(super) enum Prepared {
    Existing(CommitReceipt),
    Candidate {
        revision: u64,
        head: Box<Head>,
        receipt: CommitReceipt,
        new: bool,
    },
}

impl ExecutionStore {
    /// Initialize a NEW gate-native cancellation tree. This is deliberately not
    /// a GET-based import of a live legacy graph. Lifecycle/lease adapters must
    /// join this authority before any gated writer starts (see module docs).
    pub async fn open_gate(&self, root: OperationRef, owner: Owner) -> Result<ExecutionContext> {
        validate_reference(&root)?;
        validate_owner(&owner)?;
        let ctx = ExecutionContext::new(
            root.clone(),
            root.clone(),
            root.clone(),
            (owner.clone(), owner.clone()),
        );
        self.bind_gate_session(&root.session_id, &root).await?;
        if self.kv.get(anchor_key(&root)).await?.is_some() {
            return self.opened_context(ctx).await;
        }
        let snapshot = Snapshot {
            revision: 0,
            head: Head {
                root,
                epoch: uuid::Uuid::now_v7().to_string(),
                state: None,
                tip: None,
                checkpoint: 0,
                validation_nonce: None,
            },
        };
        let mut index = snapshot.index(self);
        let member = Member::root(ctx.operation.clone(), owner);
        index.save_member(&member).await?;
        index.install_generation(&member).await?;
        let decision = CommittedDecision {
            receipt: snapshot.receipt()?,
            previous: None,
            expected_revision: 0,
            accepted_at: Utc::now(),
            context: ctx.clone(),
            action: CommittedAction::Open,
        };
        let head = Box::pin(self.persist_candidate(&snapshot, index, decision)).await?;
        // No initializer can overwrite another initializer's winning anchor.
        let result = self
            .kv
            .create(anchor_key(&head.root), serde_json::to_vec(&head)?.into())
            .await;
        match result {
            Ok(_) => Ok(ctx),
            Err(error) => self
                .opened_context(ctx)
                .await
                .with_context(|| format!("opening gate: {error}")),
        }
    }

    async fn opened_context(&self, ctx: ExecutionContext) -> Result<ExecutionContext> {
        let initial = self.gate_decision_at(&ctx.gate_root, 1).await?;
        ensure!(
            initial.context == ctx && initial.action == CommittedAction::Open,
            "gate already initialized with another identity"
        );
        Ok(ctx)
    }

    /// Persist an exact action and payload; a successful root CAS is the commit.
    /// No handler, transcript append, or external cleanup runs inside this call.
    pub async fn commit_if_admissible(
        &self,
        ctx: &ExecutionContext,
        action: CommitAction,
    ) -> Result<CommitReceipt> {
        ctx.validate()?;
        validate_id(&action.id)?;
        ensure!(
            serde_json::to_vec(&action)?.len() < index::MAX_RECORD_BYTES / 2,
            "gate action exceeds 64 KiB"
        );
        loop {
            let snapshot = self.gate_snapshot(&ctx.gate_root).await?;
            let prepared = Box::pin(self.prepare_action(&snapshot, ctx, &action)).await?;
            if let Some(receipt) = self.publish_prepared(prepared).await? {
                return Ok(receipt);
            }
            tokio::task::yield_now().await;
        }
    }

    pub(super) async fn prepare_action(
        &self,
        snapshot: &Snapshot,
        ctx: &ExecutionContext,
        action: &CommitAction,
    ) -> Result<Prepared> {
        ensure!(snapshot.head.root == ctx.gate_root, "gate root mismatch");
        let mut index = snapshot.index(self);
        index.check_retry(ctx, &action.kind).await?;
        let key = action_key(ctx, &action.id);
        if let Some(receipt) = index.get::<CommitReceipt>(&key).await? {
            let previous = index.decision(&receipt).await?;
            ensure!(
                previous.context == *ctx
                    && previous.action
                        == CommittedAction::Action {
                            action: action.clone()
                        },
                "action identity reused with different content"
            );
            index.check_repeated_action(ctx, &action.kind).await?;
            // A duplicate is historical proof, but returning it as admissible
            // still needs a broker CAS, never a positive cached Running read.
            let mut head = snapshot.head.clone();
            head.validation_nonce = Some(uuid::Uuid::now_v7().to_string());
            return Ok(Prepared::Candidate {
                revision: snapshot.revision,
                head: Box::new(head),
                receipt,
                new: false,
            });
        }
        self.bind_action_session(ctx, &action.kind).await?;
        let receipt = snapshot.receipt()?;
        Box::pin(index.apply(ctx, &action.kind, &receipt)).await?;
        index.set(&key, &receipt).await?;
        let decision = CommittedDecision {
            receipt,
            previous: snapshot.head.tip.clone(),
            expected_revision: snapshot.revision,
            accepted_at: Utc::now(),
            context: ctx.clone(),
            action: CommittedAction::Action {
                action: action.clone(),
            },
        };
        let head = Box::pin(self.persist_candidate(snapshot, index, decision)).await?;
        Ok(Prepared::Candidate {
            revision: snapshot.revision,
            receipt: head.tip.clone().context("candidate receipt missing")?,
            head: Box::new(head),
            new: true,
        })
    }

    /// Stop one immutable scope under its governing tree, without enumerating
    /// descendants. Retry with the original scope and cancellation ID after an
    /// ambiguous acknowledgement, even after another generation was installed.
    pub async fn interrupt(
        &self,
        scope: &InterruptScope,
        cancellation_id: &str,
    ) -> Result<StopReceipt> {
        validate_reference(&scope.operation)?;
        validate_id(cancellation_id)?;
        ensure!(scope.reason.len() <= 4096, "stop reason too long");
        let decision = StopDecision {
            cancellation_id: cancellation_id.into(),
            accepted_at: Utc::now(),
            reason: scope.reason.clone(),
        };
        loop {
            let snapshot = self.gate_snapshot(&scope.gate_root).await?;
            let prepared = Box::pin(self.prepare_stop(&snapshot, scope, &decision)).await?;
            if let Some(receipt) = self.publish_prepared(prepared).await? {
                let committed = self.committed_decision(&receipt).await?;
                if let CommittedAction::Interrupt { scope, decision } = committed.action {
                    return Ok(StopReceipt {
                        scope: scope.operation,
                        decision,
                        commit: receipt,
                    });
                }
                anyhow::bail!("invalid committed stop");
            }
            tokio::task::yield_now().await;
        }
    }

    pub(super) async fn prepare_stop(
        &self,
        snapshot: &Snapshot,
        scope: &InterruptScope,
        decision: &StopDecision,
    ) -> Result<Prepared> {
        ensure!(snapshot.head.root == scope.gate_root, "gate root mismatch");
        let mut index = snapshot.index(self);
        let key = format!("cancellation/{}", decision.cancellation_id);
        if let Some(stop) = index.get::<StopReceipt>(&key).await? {
            ensure!(
                stop.scope == scope.operation,
                "cancellation identity reused for another scope"
            );
            return Ok(Prepared::Existing(stop.commit));
        }
        let mut member = index.member(&scope.operation).await?;
        if let Some(stop) = member.stop {
            return Ok(Prepared::Existing(stop.commit));
        }
        ensure!(
            member.logical != LogicalState::Completed,
            "scope already finished"
        );
        let generation = index.member(&member.generation).await?;
        let context = ExecutionContext::new(
            member.generation.clone(),
            scope.gate_root.clone(),
            member.reference.clone(),
            (member.owner.clone(), generation.owner),
        );
        let receipt = snapshot.receipt()?;
        member.logical = LogicalState::Interrupted;
        let stop = StopReceipt {
            scope: scope.operation.clone(),
            decision: decision.clone(),
            commit: receipt.clone(),
        };
        member.stop = Some(stop.clone());
        index.save_member(&member).await?;
        index.set(&key, &stop).await?;
        let committed = CommittedDecision {
            receipt,
            previous: snapshot.head.tip.clone(),
            expected_revision: snapshot.revision,
            accepted_at: decision.accepted_at,
            context,
            action: CommittedAction::Interrupt {
                scope: scope.clone(),
                decision: decision.clone(),
            },
        };
        let head = self.persist_candidate(snapshot, index, committed).await?;
        Ok(Prepared::Candidate {
            revision: snapshot.revision,
            receipt: head.tip.clone().context("candidate receipt missing")?,
            head: Box::new(head),
            new: true,
        })
    }

    pub(super) async fn publish_prepared(
        &self,
        prepared: Prepared,
    ) -> Result<Option<CommitReceipt>> {
        let Prepared::Candidate {
            revision,
            head,
            receipt,
            new,
        } = prepared
        else {
            let Prepared::Existing(receipt) = prepared else {
                unreachable!()
            };
            return Ok(Some(receipt));
        };
        match self.cas_gate(revision, &head).await {
            Ok(true) => Ok(Some(receipt)),
            Ok(false) => Ok(None),
            Err(error) => {
                // Another commit may already have advanced the head after our lost
                // acknowledgement. Immutable committed-index proof still resolves it.
                if new && self.committed_decision(&receipt).await.is_ok() {
                    return Ok(Some(receipt));
                }
                Err(error).context("gate acceptance unknown; retry the same action identity")
            }
        }
    }

    async fn bind_action_session(&self, ctx: &ExecutionContext, action: &GateAction) -> Result<()> {
        match action {
            GateAction::StartWork { child, .. } | GateAction::RegisterStoppedWork { child }
                if child.kind == crate::OperationKind::Session =>
            {
                self.bind_gate_session(&child.operation.session_id, &ctx.gate_root)
                    .await
            }
            _ => Ok(()),
        }
    }

    /// Immutable allocation, not work admission. A losing candidate may reserve
    /// an unused session name; it cannot execute. Cross-gate migration needs an
    /// explicit handover protocol and is rejected, never guessed from a GET.
    async fn bind_gate_session(&self, session: &str, root: &OperationRef) -> Result<()> {
        validate_reference(&OperationRef::new(session, "binding"))?;
        let key = format!("sessions/{session}/gate-authority");
        match self.kv.create(&key, serde_json::to_vec(root)?.into()).await {
            Ok(_) => Ok(()),
            Err(error) => {
                let bound: OperationRef = index::read(&self.kv, &key)
                    .await
                    .with_context(|| format!("gate authority allocation: {error}"))?;
                ensure!(bound == *root, "session already belongs to another gate");
                Ok(())
            }
        }
    }
}
