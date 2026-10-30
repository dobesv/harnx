//! Inject a completion between the replay lookup and its validated row read.
use super::*;
use harnx_toolset::PartialResultStore;

pub(super) async fn complete_after_first_read(
    client: &async_nats::Client,
    journal: InvocationJournal,
    invocation: ToolRequest,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let mut requests = client.subscribe("REPLAY_RACE.>").await?;
    async_nats::jetstream::new(client.clone())
        .query_account()
        .await?;
    let client = client.clone();
    Ok(tokio::spawn(async move {
        let mut completed = false;
        while let Some(request) = requests.next().await {
            let suffix = request.subject.strip_prefix("REPLAY_RACE.").unwrap();
            let payload: serde_json::Value = serde_json::from_slice(&request.payload).unwrap();
            let response = client
                .request(format!("$JS.API.{suffix}"), request.payload)
                .await
                .expect("forward JetStream request");
            let reads_invocation = suffix == "STREAM.MSG.GET.KV_harnx_tool_invocations"
                && payload["last_by_subj"]
                    .as_str()
                    .is_some_and(|subject| subject.ends_with(".failed-original"));
            if !completed && reads_invocation {
                // Hold the old row's response until the newer partial result
                // and reply are durable. No scheduling or sleep-based race.
                journal
                    .partial_result_store(&invocation)
                    .record_partial_result(json!({"job": "j-2"}))
                    .await
                    .expect("record newer partial result");
                journal
                    .complete(
                        &invocation,
                        ToolReply {
                            call_id: invocation.call_id.clone(),
                            result: Err(ToolErrorPayload::Recoverable("job failed".into())),
                            final_progress: None,
                        },
                    )
                    .await
                    .expect("complete invocation");
                completed = true;
            }
            client
                .publish(
                    request.reply.expect("JetStream reply subject"),
                    response.payload,
                )
                .await
                .expect("return proxied JetStream reply");
        }
    }))
}
