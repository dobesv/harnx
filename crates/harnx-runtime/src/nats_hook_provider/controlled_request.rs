use super::*;
use harnx_execution_control::{ExecutionStore, OperationRef};

pub(super) async fn request(
    client: &async_nats::Client,
    subject: String,
    payload: Vec<u8>,
    timeout: Duration,
) -> Result<HookOutcome> {
    let value: serde_json::Value = serde_json::from_slice(&payload)?;
    let parent = value
        .get("_harnx_execution")
        .cloned()
        .map(serde_json::from_value::<OperationRef>)
        .transpose()?;
    let mut headers = async_nats::HeaderMap::new();
    let control = if let Some(parent) = parent {
        let js = async_nats::jetstream::new(client.clone());
        let store =
            ExecutionStore::from_store(js.get_key_value(harnx_execution_control::BUCKET).await?);
        let reference = OperationRef::new(&parent.session_id, uuid::Uuid::now_v7().to_string());
        store.child(reference.clone(), parent).await?;
        headers.insert("Harnx-Hook-Operation", serde_json::to_string(&reference)?);
        Some((store, reference))
    } else {
        None
    };
    // No implicit Core NATS timeout: the explicit deadline below controls the
    // frontend while the server retains ownership of a cooperative handler.
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
        Ok(Ok(message)) => {
            serde_json::from_slice(&message.payload).context("deserialize hook reply")
        }
        error => {
            if let Some((store, reference)) = control {
                let _ = store.cancel_operation(&reference, None, false).await;
            }
            anyhow::bail!("hook request did not acknowledge completion: {error:?}")
        }
    }
}
