use crate::common; // reuse the crate's existing integration test helpers for a broker + journal
use anyhow::{Context, Result};
use common::{request_headers, TestHarness};
use harnx_toolset::{ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_reply_wins_and_later_replies_are_dropped() {
    let (_server, journal) = common::journal().await; // spawns nats-server, ensures the journal bucket
    let request: ToolRequest = common::request("sess-1", "call-1");
    journal
        .record(&request, ("echo", "scope", "srv"))
        .await
        .unwrap();
    let first = ToolReply {
        call_id: "call-1".into(),
        result: Ok(serde_json::json!({"n": 1})),
        final_progress: None,
    };
    let second = ToolReply {
        call_id: "call-1".into(),
        result: Ok(serde_json::json!({"n": 2})),
        final_progress: None,
    };
    let winner = journal.complete(&request, first.clone()).await.unwrap();
    let loser = journal.complete(&request, second).await.unwrap();
    assert_eq!(winner.result, first.result);
    assert_eq!(
        loser.result, first.result,
        "a later reply returns the durable winner"
    );
    assert_eq!(
        journal
            .completed_reply(&request)
            .await
            .unwrap()
            .unwrap()
            .result,
        first.result
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_rejects_two_rows_answering_the_same_round_and_call() {
    let (_server, journal) = common::journal().await;
    // Two dispatch attempts, keyed by their own wire ids, but both naming the
    // same transcript call — `common::request` always names it "model-call".
    let first = common::request_in_round("sess-ambiguous", "wire-1", 1);
    let second = common::request_in_round("sess-ambiguous", "wire-2", 1);
    journal
        .record(&first, ("echo", "scope", "srv-1"))
        .await
        .unwrap();
    journal
        .record(&second, ("echo", "scope", "srv-2"))
        .await
        .unwrap();

    let error = journal
        .find("sess-ambiguous", 1, "model-call")
        .await
        .expect_err("two rows answering the same round and call must be rejected");
    assert!(
        format!("{error:#}").contains("ambiguous durable tool invocation"),
        "expected an ambiguous-invocation error, got {error:#}"
    );
}

/// A cancel names its call by session and call id, so a call id that matched
/// some other call's row would have the cancel act on that call's checkpoint.
/// Ids are escaped into literal subject tokens, so wildcard characters in one
/// name no row at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wildcard_call_id_reads_no_other_calls_row() {
    let (_server, journal) = common::journal().await;
    journal
        .record(
            &common::request("sess-wildcard", "job.1"),
            ("echo", "scope", "srv"),
        )
        .await
        .unwrap();

    for call_id in ["job.*", "job.>"] {
        let read = journal.recorded("sess-wildcard", call_id).await;
        assert!(matches!(read, Ok(None)), "call id {call_id} read {read:?}");
    }
}

/// The server marks a key it removed, for an age limit say, with a header
/// rather than the KV operation header. Such a key is gone, not a row whose
/// content failed to decode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_the_server_marked_removed_reads_as_missing() -> Result<()> {
    let (server, journal) = common::journal().await;
    let client = async_nats::ConnectOptions::new()
        .token(common::TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let mut marker = async_nats::HeaderMap::new();
    marker.insert("Nats-Marker-Reason", "MaxAge");
    async_nats::jetstream::new(client)
        .publish_with_headers(
            format!(
                "$KV.{}.sessions.sess-marker.1.model-call.expired-call",
                harnx_toolset_server::invocation_journal::BUCKET
            ),
            marker,
            "".into(),
        )
        .await?
        .await?;

    let read = journal.recorded("sess-marker", "expired-call").await;

    assert!(matches!(read, Ok(None)), "the marker read as {read:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_listings_skip_a_corrupt_row_and_still_return_the_rest() {
    let (server, journal) = common::journal().await;
    let good = common::request_in_round("sess-corrupt", "wire-good", 1);
    journal
        .record(&good, ("echo", "scope", "srv-good"))
        .await
        .unwrap();

    // A row that will not deserialize as `RecordedInvocation` — written
    // directly to the bucket the journal reads, standing in for whatever
    // could leave a session with an unreadable row in production.
    let client = async_nats::ConnectOptions::new()
        .token(common::TOKEN.to_string())
        .connect(&server.url)
        .await
        .unwrap();
    let js = async_nats::jetstream::new(client);
    let store = js
        .get_key_value(harnx_toolset_server::invocation_journal::BUCKET)
        .await
        .unwrap();
    store
        .put(
            "sessions.sess-corrupt.1.model-call.wire-bad",
            "not json".into(),
        )
        .await
        .unwrap();

    let all = journal.records_for_session("sess-corrupt").await.unwrap();
    assert_eq!(
        all.iter()
            .map(|record| &record.request.call_id)
            .collect::<Vec<_>>(),
        vec!["wire-good"],
        "the corrupt row is skipped, the good one is not"
    );

    let in_round = journal
        .records_in_rounds("sess-corrupt", &[1])
        .await
        .unwrap();
    assert_eq!(
        in_round
            .iter()
            .map(|record| &record.request.call_id)
            .collect::<Vec<_>>(),
        vec!["wire-good"],
        "the filtered listing skips the same corrupt row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_records_its_checkpoint_in_the_call_row() -> Result<()> {
    let mut harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    common::wait_for_registration(&harness.client, &harness.instance_id).await?;
    let mut request = common::request("sess-2", "call-2");
    request.args = json!({"checkpoint": {"remote_session": "r-1"}});
    let message = harness
        .client
        .request_with_headers(
            harness.echo_subject(),
            request_headers(&request.call_id, &request.call_id),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert_eq!(reply.result.unwrap(), request.args);

    let journal =
        InvocationJournal::ensure(&async_nats::jetstream::new(harness.client.clone()), 1).await?;
    let record = journal
        .recorded("sess-2", "call-2")
        .await?
        .context("journal row")?;
    assert_eq!(
        record.checkpoint,
        Some(json!({"remote_session": "r-1"})),
        "the handle a tool published is readable without its invocation"
    );
    harness.shutdown().await;
    Ok(())
}
