//! Cancellation publication is cleanup work, never a wait on the original RPC.
use super::*;
use harnx_execution_control::{CleanupTasks, ExecutionStore};

struct CancellationContext<'a> {
    request: &'a ToolRequest,
    server: &'a str,
    id: &'a str,
}

impl NatsToolProvider {
    pub(super) fn schedule_cancel(&self, request: &ToolRequest, server: &str) {
        let request = request.clone();
        let server = server.to_owned();
        let client = self.client.clone();
        let subject = self.instance_id.control_subject();
        let cancellation_id = Uuid::now_v7().to_string();
        CleanupTasks::process().spawn(async move {
            let cancellation = CancellationContext {
                request: &request,
                server: &server,
                id: &cancellation_id,
            };
            let mut backoff = Duration::from_millis(100);
            loop {
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    cancel_request(&client, &subject, &cancellation),
                )
                .await;
                if matches!(result, Ok(Ok(()))) {
                    return;
                }
                log::debug!(
                    "tool cancellation wake-up failed; durable reconciler also retries: {result:?}"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
    }
}

async fn cancel_request(
    client: &async_nats::Client,
    subject: &str,
    cancellation: &CancellationContext<'_>,
) -> anyhow::Result<()> {
    let CancellationContext {
        request,
        server,
        id,
    } = *cancellation;
    let js = async_nats::jetstream::new(client.clone());
    let store =
        ExecutionStore::from_store(js.get_key_value(harnx_execution_control::BUCKET).await?);
    let original = match request
        .replay_execution
        .as_ref()
        .or(request.execution.as_ref())
    {
        Some(execution) => execution.clone(),
        None => {
            // Direct calls get their original receiver at server admission. Do
            // not infer a session's current generation for this delayed cancel.
            let journal =
                harnx_toolset_server::invocation_journal::InvocationJournal::ensure(&js).await?;
            journal
                .get(request)
                .await?
                .context("tool cancellation has no original request")?
                .request
                .execution
                .context("tool cancellation identity missing")?
        }
    };
    store
        .cancel_operation(original.producer.operation(), Some(id), false)
        .await?;
    let control = ControlMessage::cancel(original.producer, server.into(), id.into());
    client
        .publish(subject.to_owned(), serde_json::to_vec(&control)?.into())
        .await?;
    client.flush().await?;
    Ok(())
}
