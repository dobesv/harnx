//! A call's partial result lives on its journal row: the latest value stands
//! until a reply decides the call, and a tool records it through the store the
//! server hands it.
use crate::common::{self, request_headers, TestHarness};
use anyhow::{Context, Result};
use harnx_toolset::{
    CheckpointStore, PartialResultStore, ToolErrorPayload, ToolReply, ToolRequest,
};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_latest_partial_result_stands_until_the_call_is_answered() {
    let (_server, journal) = common::journal().await;
    let request = common::request("sess-partial", "call-partial");
    journal
        .record(&request, ("echo", "scope", "srv"))
        .await
        .unwrap();
    let store = journal.partial_result_store(&request);

    store
        .record_partial_result(json!({"step": 1}))
        .await
        .unwrap();
    store
        .record_partial_result(json!({"step": 2}))
        .await
        .unwrap();
    journal
        .complete(
            &request,
            ToolReply {
                call_id: request.call_id.clone(),
                result: Err(ToolErrorPayload::Recoverable("failed".into())),
                final_progress: None,
            },
        )
        .await
        .unwrap();
    store
        .record_partial_result(json!({"step": 3}))
        .await
        .unwrap();

    let row = journal
        .get(&request)
        .await
        .unwrap()
        .expect("the call's row");
    assert_eq!(
        row.partial_result,
        Some(json!({"step": 2})),
        "a value recorded after the reply describes a call that is already over"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standalone_call_keeps_its_partial_result_on_its_own_row() {
    let (_server, journal) = common::journal().await;
    let request = ToolRequest {
        run_context: None,
        parent_session_id: None,
        ..common::request("unused", "standalone-partial")
    };
    journal
        .record(&request, ("echo", "scope", "srv"))
        .await
        .unwrap();

    journal
        .partial_result_store(&request)
        .record_partial_result(json!({"step": 1}))
        .await
        .unwrap();

    let row = journal
        .get(&request)
        .await
        .unwrap()
        .expect("the standalone row");
    assert_eq!(row.partial_result, Some(json!({"step": 1})));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_tool_leaves_its_partial_result_on_the_call_row() -> Result<()> {
    let mut harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    common::wait_for_registration(&harness.client, &harness.instance_id).await?;
    let mut request = common::request("sess-partial-wire", "call-partial-wire");
    request.args = json!({"partial_result": {"job": "j-1"}, "error": "job failed"});
    let message = harness
        .client
        .request_with_headers(
            harness.echo_subject(),
            request_headers(&request.call_id, &request.call_id),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert_eq!(
        reply.result,
        Err(ToolErrorPayload::Recoverable("job failed".into()))
    );

    let journal =
        InvocationJournal::ensure(&async_nats::jetstream::new(harness.client.clone()), 1).await?;
    let row = journal
        .recorded("sess-partial-wire", "call-partial-wire")
        .await?
        .context("journal row")?;
    assert_eq!(row.partial_result, Some(json!({"job": "j-1"})));
    harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_checkpoint_and_partial_result_updates_preserve_both_fields() -> Result<()> {
    let (_server, journal) = common::journal().await;
    let request = common::request("sess-concurrent", "call-concurrent");
    journal.record(&request, ("echo", "scope", "srv")).await?;

    let handles = futures_util::future::try_join_all((0..8).map(|index| {
        let checkpoint = journal.checkpoint_store(&request);
        let partial = journal.partial_result_store(&request);
        async move {
            let (handle, ()) = tokio::try_join!(
                checkpoint.checkpoint(json!({"job": index})),
                partial.record_partial_result(json!({"step": index})),
            )?;
            Ok::<_, anyhow::Error>(handle)
        }
    }))
    .await?;
    let winner = &handles[0];
    assert!(handles.iter().all(|handle| handle == winner));

    let row = journal.get(&request).await?.context("call row")?;
    assert_eq!(row.checkpoint.as_ref(), Some(winner));
    let partial = row
        .partial_result
        .context("partial result survived CAS retries")?;
    journal
        .partial_result_store(&request)
        .record_partial_result(partial.clone())
        .await?;
    assert_eq!(
        journal
            .checkpoint_store(&request)
            .checkpoint(json!({"job": "later"}))
            .await?,
        *winner,
    );
    let unchanged = journal.get(&request).await?.context("unchanged call row")?;
    assert_eq!(unchanged.partial_result, Some(partial));
    assert_eq!(unchanged.checkpoint.as_ref(), Some(winner));
    Ok(())
}
