//! Controlled hooks retain their invocation future through cancellation.
use super::*;
use harnx_execution_control::{ExecutionStore, OperationRef, Owner};

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
    let reference: OperationRef = serde_json::from_str(header.as_str())?;
    let js = jetstream::new(client.clone());
    let store =
        ExecutionStore::from_store(js.get_key_value(harnx_execution_control::BUCKET).await?);
    let owner = Owner::invocation(&format!("hook-{}", hook.name()));
    store.claim(&reference, owner.clone()).await?;
    let payload: HookPayload = serde_json::from_slice(&message.payload)?;
    let future = hook.handle_hook(payload);
    tokio::pin!(future);
    let outcome = if store.check_ancestors(&reference).await.is_err() {
        store.cancel_operation(&reference, None, false).await?;
        continue_outcome()
    } else {
        tokio::select! {
            outcome = &mut future => outcome,
            _ = store.watch_cancellation(&reference) => {
                store.cancel_operation(&reference, None, false).await?;
                store.quiesce(&reference, &owner).await?;
                // Hook has no per-invocation cancellation guarantee. Keep the
                // future alive and expose unconfirmed until its owner returns.
                future.await
            }
        }
    };
    store.owner_stopped(&reference, &owner).await?;
    if let Ok(reply) = harnx_nats_common::rpc::ReplyTarget::from_message(&message) {
        reply.send(&client, serde_json::to_vec(&outcome)?).await?;
    }
    Ok(())
}
