//! A call that wind-up answers without a success carries the partial result
//! its tool recorded; a call that succeeded answers with its own result alone.
use super::*;
use harnx_toolset::{PartialResultStore, ToolErrorPayload};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_calls_that_did_not_succeed_answer_with_their_partial_result() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let calls = ["running", "failed", "succeeded"];
    let turn = InterruptedTurn::seed(server.url(), &calls).await;
    for call_id in calls {
        let request = turn.request(call_id);
        turn.dispatched(&request, "srv-1").await;
        turn.journal
            .partial_result_store(&request)
            .record_partial_result(serde_json::json!({"job": call_id}))
            .await
            .unwrap();
    }
    let failed = turn.request("failed");
    turn.journal
        .complete(
            &failed,
            ToolReply {
                call_id: failed.call_id.clone(),
                result: Err(ToolErrorPayload::Recoverable("job failed".into())),
                final_progress: None,
            },
        )
        .await
        .unwrap();
    turn.answered(
        &turn.request("succeeded"),
        serde_json::json!({"done": true}),
    )
    .await;

    turn.wind_up(&NatsInFlightCalls::default()).await;

    let (_, results) = turn.wound_results().await;
    assert_eq!(
        answer(&results, "running"),
        &serde_json::json!({
            "error": INTERRUPTED_TOOL_RESPONSE_ERROR,
            "cancellation_id": CANCELLATION_ID,
            "partial_result": {"job": "running"},
        }),
        "an interrupted call's placeholder names what it had produced"
    );
    assert_eq!(
        answer(&results, "failed"),
        &serde_json::json!({"error": "job failed", "partial_result": {"job": "failed"}})
    );
    assert_eq!(
        answer(&results, "succeeded"),
        &serde_json::json!({"done": true}),
        "a successful call answers with its own result alone"
    );
}
