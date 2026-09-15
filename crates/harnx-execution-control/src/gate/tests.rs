use super::{engine::Prepared, ledger, *};
use crate::{test_common, CleanupState, ExecutionStore, OperationKind, OperationRef, Owner};
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{oneshot, Barrier};

mod bridge;
mod cleanup;
mod compaction;
mod payload;
mod races;
mod recovery;
mod validation;

async fn stores() -> Result<(
    test_common::NatsServerHandle,
    ExecutionStore,
    ExecutionStore,
)> {
    let server = test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let first = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let second = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    Ok((
        server,
        ExecutionStore::ensure(&first, 1).await?,
        ExecutionStore::ensure(&second, 1).await?,
    ))
}

fn owner(fence: u64) -> Owner {
    Owner {
        instance_id: format!("worker-{fence}"),
        fence,
    }
}
async fn root(store: &ExecutionStore) -> Result<ExecutionContext> {
    store
        .open_gate(OperationRef::new("gate-session", "g1"), owner(1))
        .await
}
fn action(id: &str, kind: GateAction) -> CommitAction {
    CommitAction {
        id: id.into(),
        kind,
    }
}
fn output(id: &str, kind: OutputKind) -> CommitAction {
    action(
        id,
        GateAction::CommitOutput {
            output: CommittedOutput {
                id: id.into(),
                kind,
                payload: json!({"result": id}),
            },
        },
    )
}
fn scope(ctx: &ExecutionContext) -> InterruptScope {
    InterruptScope {
        gate_root: ctx.gate_root.clone(),
        operation: ctx.operation.clone(),
        reason: "user interrupt".into(),
    }
}
fn child(ctx: &ExecutionContext, id: &str, kind: OperationKind) -> WorkRegistration {
    let session = if kind == OperationKind::Tool {
        &ctx.generation.session_id
    } else {
        id
    };
    WorkRegistration {
        operation: OperationRef::new(session, id),
        kind,
        owner: owner(1),
    }
}
async fn start(
    store: &ExecutionStore,
    ctx: &ExecutionContext,
    id: &str,
    kind: OperationKind,
) -> Result<ExecutionContext> {
    let child = child(ctx, id, kind);
    store
        .commit_if_admissible(
            ctx,
            action(
                id,
                GateAction::StartWork {
                    child: child.clone(),
                    input: json!({"call": id}),
                },
            ),
        )
        .await?;
    Ok(child.context(ctx))
}
async fn replace(store: &ExecutionStore, ctx: &ExecutionContext) -> Result<ExecutionContext> {
    let generation = OperationRef::new(&ctx.generation.session_id, "g2");
    store
        .commit_if_admissible(
            ctx,
            action(
                "replace",
                GateAction::ReplaceGeneration {
                    generation: generation.clone(),
                    owner: owner(2),
                },
            ),
        )
        .await?;
    Ok(ExecutionContext::new(
        generation.clone(),
        ctx.gate_root.clone(),
        generation,
        (owner(2), owner(2)),
    ))
}

struct PausedCommit {
    receipt: CommitReceipt,
    release: Arc<Barrier>,
    task: tokio::task::JoinHandle<Result<Option<CommitReceipt>>>,
}
impl PausedCommit {
    async fn publish(self) -> Result<Option<CommitReceipt>> {
        self.release.wait().await;
        self.task.await?
    }
}

/// Pause AFTER immutable candidate persistence but BEFORE the root CAS. Separate
/// NATS clients perform the competing change; no local mutex or sleeps order it.
async fn pause(
    store: ExecutionStore,
    ctx: ExecutionContext,
    action: CommitAction,
) -> Result<PausedCommit> {
    let release = Arc::new(Barrier::new(2));
    let barrier = release.clone();
    let (sender, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        let snapshot = store.gate_snapshot(&ctx.gate_root).await?;
        let prepared = store.prepare_action(&snapshot, &ctx, &action).await?;
        let receipt = match &prepared {
            Prepared::Existing(receipt) | Prepared::Candidate { receipt, .. } => receipt.clone(),
        };
        sender
            .send(receipt)
            .map_err(|_| anyhow::anyhow!("test receiver closed"))?;
        barrier.wait().await;
        store.publish_prepared(prepared).await
    });
    let receipt = receiver.await?;
    Ok(PausedCommit {
        receipt,
        release,
        task,
    })
}
