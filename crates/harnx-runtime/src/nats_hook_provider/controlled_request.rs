use super::*;
use crate::execution_fence::GenerationFence;
use harnx_execution_control::{
    CommitAction, CommitReceipt, ExecutionContext, ExecutionStore, GateAction, Interrupted,
    OperationRef,
};

pub(super) async fn request(
    client: &async_nats::Client,
    subject: String,
    payload: Vec<u8>,
    options: HookRequestOptions,
) -> Result<HookOutcome> {
    let HookRequestOptions { timeout, abort } = options;
    ensure_local(&abort)?;
    let value: serde_json::Value = serde_json::from_slice(&payload)?;
    let parent = value
        .get("_harnx_execution")
        .cloned()
        .map(serde_json::from_value::<ExecutionContext>)
        .transpose()?;
    let mut headers = async_nats::HeaderMap::new();
    let control = if let Some(parent) = parent {
        let js = async_nats::jetstream::new(client.clone());
        let store =
            ExecutionStore::from_store(js.get_key_value(harnx_execution_control::BUCKET).await?);
        let fence = GenerationFence::new(store.clone(), parent.clone());
        fence.check("hook-registration").await?;
        let reference = OperationRef::new(
            &parent.generation().session_id,
            uuid::Uuid::now_v7().to_string(),
        );
        store
            .child(reference.clone(), parent.operation().clone())
            .await?;
        // Activation commits StartWork and is arbitrated against physical cancel.
        let producer = Box::pin(store.activate_gate(&reference)).await?;
        headers.insert("Harnx-Hook-Operation", serde_json::to_string(&producer)?);
        fence.check("hook-handoff").await?;
        Some((fence, reference))
    } else {
        None
    };
    ensure_local(&abort)?;
    // Compatibility: retain the handler through physical cleanup. No early return.
    let request = async_nats::Request::new()
        .payload(payload.into())
        .headers(headers)
        .timeout(None);
    let response = tokio::time::timeout(
        timeout,
        harnx_nats_common::rpc::request(client, subject, request),
    )
    .await;
    match response {
        Ok(Ok(message)) => match control {
            Some((fence, _)) => {
                ensure_local(&abort)?;
                consume(&fence, &message.payload).await
            }
            None => serde_json::from_slice(&message.payload).context("deserialize hook reply"),
        },
        error => {
            if let Some((fence, reference)) = control {
                let _ = fence.store.cancel_operation(&reference, None, false).await;
            }
            anyhow::bail!("hook request did not acknowledge completion: {error:?}")
        }
    }
}

async fn consume(fence: &GenerationFence, payload: &[u8]) -> Result<HookOutcome> {
    let value: serde_json::Value = serde_json::from_slice(payload)?;
    if let Some(interrupted) = value.get("interrupted") {
        return Err(serde_json::from_value::<Interrupted>(interrupted.clone())?.into());
    }
    let producer: ExecutionContext = serde_json::from_value(value["producer"].clone())?;
    let receipt: CommitReceipt = serde_json::from_value(value["commit"].clone())?;
    let outcome = fence.store.committed_output_payload(&receipt).await?;
    anyhow::ensure!(
        outcome == value["outcome"],
        "hook reply differs from committed output"
    );
    fence
        .store
        .commit_if_admissible(
            &fence.context,
            CommitAction {
                id: format!("hook-consume-{}", receipt.commit_id),
                kind: GateAction::ConsumeReply {
                    producer,
                    reply: receipt,
                },
            },
        )
        .await?;
    Ok(serde_json::from_value(outcome)?)
}

fn ensure_local(abort: &Option<harnx_core::abort::AbortSignal>) -> Result<()> {
    anyhow::ensure!(
        !abort.as_ref().is_some_and(|abort| abort.aborted()),
        "hook invocation interrupted"
    );
    Ok(())
}
