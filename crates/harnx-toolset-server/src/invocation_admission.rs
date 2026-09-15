//! Bind requests at invocation creation. Legacy saved records are never upgraded
//! by guessing a current generation. Direct clients get an explicit local receiver.
use super::*;
use anyhow::ensure;
use harnx_execution_control::{ExecutionStore, OperationRef, Owner};
use harnx_toolset::ToolExecution;

/// Called by workers before journaling/dispatch, not on the reply path.
pub async fn capture(store: &ExecutionStore, reference: &OperationRef) -> Result<ToolExecution> {
    // Poll broker control outside the deep model/tool stack. Dropping the caller
    // aborts this metadata-only task; no handler is dispatched until it returns.
    let store = store.clone();
    let reference = reference.clone();
    tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
        async move { capture_inner(&store, &reference).await }.instrument(tracing::Span::current()),
    ))
    .await?
}

async fn capture_inner(store: &ExecutionStore, reference: &OperationRef) -> Result<ToolExecution> {
    let operation = store
        .get(reference)
        .await?
        .context("tool operation missing")?;
    let parent = operation.parent.context("tool receiver missing")?;
    let consumer = store.activate_gate(&parent).await?;
    let producer = store.activate_gate(reference).await?;
    Ok(ToolExecution { producer, consumer })
}

pub(super) async fn prepare(context: &ToolRequestContext, request: &mut ToolRequest) -> Result<()> {
    if request.execution.is_some() {
        return Ok(());
    }
    ensure!(
        request.replay.is_none() && request.replay_execution.is_none(),
        "legacy invocation has no retained generation authority"
    );
    if let Some(record) = context.journal.get(request).await? {
        // Direct-client duplicates recover the identity allocated for this exact
        // call, never the current session. Worker requests must carry it themselves.
        ensure!(
            record.tool_round == 0,
            "worker invocation omitted execution identity"
        );
        request.execution = record.request.execution;
        reply_fence::identity(request)?;
        return Ok(());
    }
    let store = &context.execution_store;
    let session = request
        .parent_session_id
        .as_deref()
        .unwrap_or(&request.call_id);
    let reference = OperationRef::new(session, &request.operation_id);
    if store.get(&reference).await?.is_none() {
        let root = match store.current(session).await? {
            Some(root) => {
                ensure!(
                    root.owner
                        .as_ref()
                        .is_some_and(|owner| owner.instance_id.starts_with("direct-tool-client:")),
                    "tool operation was not registered by its execution owner"
                );
                root
            }
            None => {
                let root = store
                    .session(
                        session,
                        None,
                        Some(&format!("receiver-{}", request.call_id)),
                    )
                    .await?;
                store
                    .claim(&root.reference, Owner::invocation("direct-tool-client"))
                    .await?;
                root
            }
        };
        store.child(reference.clone(), root.reference).await?;
    }
    request.execution = Some(capture(store, &reference).await?);
    context
        .journal
        .record(
            request,
            (
                &request.tool,
                context.server_scope.as_str(),
                &context.server_identity,
            ),
            0,
        )
        .await?;
    Ok(())
}

/// A fresh replay attempt uses the original operation/generation, with a new
/// owner fence. Handing off the producer and admitting its work race the same stop.
pub async fn prepare_replay(
    store: &ExecutionStore,
    request: &mut ToolRequest,
    consumer: harnx_execution_control::ExecutionContext,
) -> Result<()> {
    let original = request
        .execution
        .as_ref()
        .context("legacy replay has no generation authority")?;
    reply_fence::check_stop(store, original).await?;
    ensure!(
        consumer.operation() == original.consumer.operation()
            && consumer.generation() == original.consumer.generation(),
        "replay cannot adopt another generation"
    );
    let producer = store
        .gate_context(original.producer.gate_root(), original.producer.operation())
        .await?;
    let mut owner = Owner::invocation("tool-replay");
    owner.fence = producer
        .owner()
        .fence
        .checked_add(1)
        .context("tool owner fence exhausted")?;
    store
        .commit_if_admissible(
            &producer,
            harnx_execution_control::CommitAction {
                id: format!("replay-owner-{}", owner.instance_id),
                kind: harnx_execution_control::GateAction::ReplaceOwner {
                    owner: owner.clone(),
                },
            },
        )
        .await?;
    let producer = harnx_execution_control::ExecutionContext::new(
        producer.generation().clone(),
        producer.gate_root().clone(),
        producer.operation().clone(),
        (owner, consumer.generation_owner().clone()),
    );
    request.replay_execution = Some(ToolExecution { producer, consumer });
    Ok(())
}
