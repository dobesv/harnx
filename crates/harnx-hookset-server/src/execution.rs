//! Controlled hooks retain their invocation future through cancellation.
use super::*;
use harnx_execution_control::{
    CommitAction, ExecutionContext, ExecutionStore, GateAction, Interrupted, OutputKind, Owner,
};

pub(super) async fn handle(
    client: async_nats::Client,
    hook: Arc<dyn Hook>,
    message: async_nats::Message,
) -> Result<()> {
    let header = message
        .headers
        .as_ref()
        .and_then(|h| h.get("Harnx-Hook-Operation"))
        .context("hook operation header missing")?;
    let original: ExecutionContext = serde_json::from_str(header.as_str())?;
    let js = jetstream::new(client.clone());
    let store =
        ExecutionStore::from_store(js.get_key_value(harnx_execution_control::BUCKET).await?);
    let result = run(&store, &original, hook, &message.payload).await;
    let response = match result {
        Ok(response) => response,
        Err(error) => match error.downcast::<Interrupted>() {
            Ok(interrupted) => serde_json::json!({"interrupted": interrupted}),
            Err(error) => return Err(error),
        },
    };
    if let Ok(reply) = harnx_nats_common::rpc::ReplyTarget::from_message(&message) {
        reply.send(&client, serde_json::to_vec(&response)?).await?;
    }
    Ok(())
}

async fn run(
    store: &ExecutionStore,
    original: &ExecutionContext,
    hook: Arc<dyn Hook>,
    payload: &[u8],
) -> Result<serde_json::Value> {
    let mut owner = Owner::invocation(&format!("hook-{}", hook.name()));
    owner.fence = original
        .owner()
        .fence
        .checked_add(1)
        .context("hook owner fence exhausted")?;
    store
        .commit_if_admissible(
            original,
            CommitAction {
                id: format!("hook-owner-{}", owner.instance_id),
                kind: GateAction::ReplaceOwner {
                    owner: owner.clone(),
                },
            },
        )
        .await?;
    let producer = ExecutionContext::new(
        original.generation().clone(),
        original.gate_root().clone(),
        original.operation().clone(),
        (owner.clone(), original.generation_owner().clone()),
    );
    store.claim(producer.operation(), owner.clone()).await?;
    let result = invoke(store, &producer, hook, payload).await;
    // Physical cleanup bookkeeping is allowed after stop, never hook output.
    store.owner_stopped(producer.operation(), &owner).await?;
    result
}

async fn invoke(
    store: &ExecutionStore,
    producer: &ExecutionContext,
    hook: Arc<dyn Hook>,
    payload: &[u8],
) -> Result<serde_json::Value> {
    let payload: HookPayload = serde_json::from_slice(payload)?;
    let input = store
        .commit_blob_output(
            producer,
            harnx_execution_control::CommittedOutput {
                id: "hook-input".into(),
                kind: OutputKind::Progress,
                payload: serde_json::to_value(&payload)?,
            },
        )
        .await?;
    store
        .commit_if_admissible(
            producer,
            CommitAction {
                id: "hook-invoke".into(),
                kind: GateAction::AdmitWork {
                    input: serde_json::json!({"committed_input": input}),
                },
            },
        )
        .await?;
    let future = hook.handle_hook(payload);
    tokio::pin!(future);
    let outcome = tokio::select! {
        outcome = &mut future => outcome,
        _ = store.watch_cancellation(producer.operation()) => {
            store.cancel_operation(producer.operation(), None, false).await?;
            store.quiesce(producer.operation(), producer.owner()).await?;
            // No shutdown claim until the cooperative handler settles (Stage 7).
            future.await
        }
    };
    let commit = store
        .commit_blob_output(
            producer,
            harnx_execution_control::CommittedOutput {
                id: "hook-reply".into(),
                kind: OutputKind::ToolReply,
                payload: serde_json::to_value(&outcome)?,
            },
        )
        .await?;
    Ok(serde_json::json!({"producer": producer, "commit": commit, "outcome": outcome}))
}
