//! Shared completion/consumption adapter. A cache value or journal row carries
//! historical proof; only a fresh ConsumeReply CAS authorizes returning its bytes.
use anyhow::{ensure, Context, Result};
use harnx_execution_control::{
    CommitAction, CommitReceipt, ExecutionContext, ExecutionStore, GateAction, Interrupted,
};
use harnx_toolset::{ToolExecution, ToolInvokeError, ToolReply, ToolRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommittedReply {
    pub producer: ExecutionContext,
    pub receipt: CommitReceipt,
    pub reply: ToolReply,
}

pub fn identity(request: &ToolRequest) -> Result<&ToolExecution> {
    let execution = request
        .replay_execution
        .as_ref()
        .or(request.execution.as_ref())
        .context("invocation has no retained generation authority")?;
    ensure!(
        execution.producer.operation().execution_id == request.operation_id
            && request.operation_id == request.call_id
            && execution.producer.operation().session_id
                == request
                    .parent_session_id
                    .as_deref()
                    .unwrap_or(&request.call_id),
        "tool execution identity mismatch"
    );
    let original = request
        .execution
        .as_ref()
        .context("original generation authority missing")?;
    ensure!(
        execution.producer.operation() == original.producer.operation()
            && execution.producer.generation() == original.producer.generation()
            && execution.producer.gate_root() == original.producer.gate_root()
            && execution.consumer.operation() == original.consumer.operation()
            && execution.consumer.generation() == original.consumer.generation()
            && execution.consumer.gate_root() == original.consumer.gate_root(),
        "replay execution identity changed"
    );
    Ok(execution)
}

/// Legacy records can prove a stop by exact operation identity, but missing
/// metadata never supplies positive permission to consume or replay.
pub async fn check_request_stop(store: &ExecutionStore, request: &ToolRequest) -> Result<()> {
    if let Some(execution) = &request.execution {
        return check_stop(store, execution).await;
    }
    let session = request
        .parent_session_id
        .as_deref()
        .unwrap_or(&request.call_id);
    if let Some(root) = store.gate_root(session).await? {
        let operation = harnx_execution_control::OperationRef::new(session, &request.operation_id);
        if let Some(stop) = store.gate_stop(&root, &operation).await? {
            return Err(Interrupted { stop }.into());
        }
    }
    anyhow::bail!("legacy invocation has no retained generation authority")
}

pub async fn check_stop(store: &ExecutionStore, execution: &ToolExecution) -> Result<()> {
    // Negative authority only. Success still needs AdmitWork/ConsumeReply CAS.

    for context in [&execution.producer, &execution.consumer] {
        if let Some(stop) = store
            .gate_stop(context.gate_root(), context.operation())
            .await?
        {
            return Err(Interrupted { stop }.into());
        }
    }
    Ok(())
}

pub async fn admit_recovery(store: &ExecutionStore, request: &ToolRequest) -> Result<()> {
    check_request_stop(store, request).await?;
    let execution = identity(request)?;
    store
        .commit_if_admissible(
            &execution.consumer,
            CommitAction {
                id: format!("recover-{}", digest(&serde_json::to_vec(execution)?)),
                kind: GateAction::AdmitRecovery {
                    original: execution.producer.clone(),
                },
            },
        )
        .await?;
    Ok(())
}

pub async fn admit(store: &ExecutionStore, request: &ToolRequest) -> Result<()> {
    admit_producer(store, request, &identity(request)?.producer).await
}

pub(crate) async fn admit_producer(
    store: &ExecutionStore,
    request: &ToolRequest,
    producer: &ExecutionContext,
) -> Result<()> {
    store.commit_if_admissible(producer, CommitAction {
        id: format!("invoke-{}", producer.owner().instance_id),
        kind: GateAction::AdmitWork { input: serde_json::json!({"request_sha256": digest(&serde_json::to_vec(request)?)}) },
    }).await?;
    Ok(())
}

pub(crate) async fn claim_producer(
    store: &ExecutionStore,
    request: &ToolRequest,
    server: &str,
) -> Result<ExecutionContext> {
    let producer = &identity(request)?.producer;
    let mut owner = harnx_execution_control::Owner::invocation(server);
    owner.fence = producer
        .owner()
        .fence
        .checked_add(1)
        .context("tool owner fence exhausted")?;
    store
        .commit_if_admissible(
            producer,
            CommitAction {
                id: format!("claim-{}", owner.instance_id),
                kind: GateAction::ReplaceOwner {
                    owner: owner.clone(),
                },
            },
        )
        .await?;
    Ok(ExecutionContext::new(
        producer.generation().clone(),
        producer.gate_root().clone(),
        producer.operation().clone(),
        (owner, producer.generation_owner().clone()),
    ))
}

pub async fn consume(
    store: &ExecutionStore,
    request: &ToolRequest,
    saved: &CommittedReply,
) -> Result<ToolReply> {
    let execution = identity(request)?;
    check_stop(store, execution).await?;
    ensure!(
        saved.producer.operation() == execution.producer.operation()
            && saved.producer.generation() == execution.producer.generation()
            && saved.reply.call_id == request.call_id,
        "saved reply identity mismatch"
    );
    verify(store, saved).await?;
    store
        .commit_if_admissible(
            &execution.consumer,
            CommitAction {
                id: format!(
                    "consume-{}-{}",
                    request.call_id,
                    execution.consumer.owner().fence
                ),
                kind: GateAction::ConsumeReply {
                    producer: saved.producer.clone(),
                    reply: saved.receipt.clone(),
                },
            },
        )
        .await?;
    Ok(saved.reply.clone())
}

pub fn invoke_error(error: anyhow::Error) -> ToolInvokeError {
    match error.downcast::<Interrupted>() {
        Ok(interrupted) => ToolInvokeError::Interrupted(Box::new(interrupted)),
        Err(error) => ToolInvokeError::Fatal(format!("tool invocation recovery: {error:#}")),
    }
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut hex, "{byte:02x}").expect("writing into a String");
    }
    hex
}
pub(crate) fn payload(reply: &ToolReply) -> Result<serde_json::Value> {
    Ok(serde_json::json!({"reply_sha256": digest(&serde_json::to_vec(reply)?)}))
}
pub async fn verify(store: &ExecutionStore, saved: &CommittedReply) -> Result<()> {
    use harnx_execution_control::{CommittedAction, CommittedOutput, OutputKind};
    let decision = store.committed_decision(&saved.receipt).await?;
    ensure!(
        decision.context == saved.producer,
        "reply proof producer mismatch"
    );
    let CommittedAction::Action {
        action:
            CommitAction {
                kind:
                    GateAction::CommitOutput {
                        output:
                            CommittedOutput {
                                kind: OutputKind::ToolReply,
                                payload: committed,
                                ..
                            },
                    },
                ..
            },
    } = decision.action
    else {
        anyhow::bail!("commit is not a tool reply");
    };
    ensure!(
        committed == payload(&saved.reply)?,
        "committed reply payload mismatch"
    );
    Ok(())
}
