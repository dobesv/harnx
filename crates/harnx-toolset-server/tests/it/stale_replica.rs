//! A JetStream follower answers direct gets from whatever it has applied so
//! far, and NATS can place a consumer on any replica, so on a replicated
//! bucket a read made right after a write can miss it. These tests run
//! against a broker that keeps both kinds of request for the invocation
//! journal's stream away from it. A direct get reaches a stream that has
//! applied none of the journal's writes and answers "not found", just as a
//! lagging follower does, and consumer creation reaches nothing at all. Only
//! requests that the stream leader alone answers still reach the journal.
use crate::common;
use anyhow::{Context, Result};
use async_nats::jetstream::{self, stream};
use common::{wait_for_registration, NatsServerHandle, TestHarness, TestToolset};
use harnx_toolset::{CheckpointStore, ReplayAttempt, ToolErrorPayload, ToolReply};
use harnx_toolset_server::invocation_journal::{self, InvocationJournal};
use serde_json::json;
use std::sync::atomic::Ordering;

/// Answers the journal's direct gets in place of the journal's own stream.
/// Nothing publishes to its subject, so it never holds a row.
const STALE_REPLICA: &str = "STALE_JOURNAL_REPLICA";

fn stale_replica_config() -> String {
    let journal = format!("KV_{}", invocation_journal::BUCKET);
    format!(
        r#"mappings = {{
  "$JS.API.DIRECT.GET.{journal}": "$JS.API.DIRECT.GET.{STALE_REPLICA}"
  "$JS.API.DIRECT.GET.{journal}.>": "$JS.API.DIRECT.GET.{STALE_REPLICA}.>"
  "$JS.API.CONSUMER.CREATE.{journal}": "harnx.test.unanswered"
  "$JS.API.CONSUMER.CREATE.{journal}.>": "harnx.test.unanswered.>"
}}
"#
    )
}

/// Start a broker whose replicas all lag the journal, and open the journal on
/// it the way a worker does.
async fn stale_replica_broker() -> Result<(NatsServerHandle, InvocationJournal)> {
    let server = common::spawn_configured_nats_server(Some(&stale_replica_config()))
        .await?
        .context("nats-server required")?;
    let client = async_nats::ConnectOptions::new()
        .token(common::TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let journal = lagging_journal(&jetstream::new(client)).await?;
    Ok((server, journal))
}

/// Serve `toolset` on a broker whose replicas all lag the journal.
async fn serve(toolset: TestToolset) -> Result<(TestHarness, InvocationJournal)> {
    let harness = TestHarness::with_broker_config(toolset, None, Some(&stale_replica_config()))
        .await?
        .context("nats-server required")?;
    let journal = lagging_journal(&jetstream::new(harness.client.clone())).await?;
    wait_for_registration(&harness.client, &harness.instance_id).await?;
    Ok((harness, journal))
}

/// Create the replica that answers for the journal, then open the journal the
/// way a worker does.
async fn lagging_journal(js: &jetstream::Context) -> Result<InvocationJournal> {
    js.create_stream(stream::Config {
        name: STALE_REPLICA.into(),
        subjects: vec!["harnx.test.stale-replica".into()],
        allow_direct: true,
        storage: stream::StorageType::Memory,
        ..Default::default()
    })
    .await?;
    let journal = InvocationJournal::ensure(js, 1).await?;
    assert_replica_reads_miss_writes(js).await?;
    Ok(journal)
}

/// Without this, a broker that stopped diverting those requests would let
/// every test here pass without a lagging replica in the way.
async fn assert_replica_reads_miss_writes(js: &jetstream::Context) -> Result<()> {
    let store = js.get_key_value(invocation_journal::BUCKET).await?;
    store.put("stale-replica-probe", "written".into()).await?;
    anyhow::ensure!(
        store.get("stale-replica-probe").await?.is_none(),
        "a direct get saw a write the stale replica never applied"
    );
    anyhow::ensure!(
        store.keys().await.is_err(),
        "a consumer listed the journal despite the broker diverting it"
    );
    Ok(())
}

/// Journal `request` as a worker does before it dispatches the call.
async fn journal_call(
    journal: &InvocationJournal,
    request: &harnx_toolset::ToolRequest,
) -> Result<()> {
    journal
        .record(request, ("test_echo", "worker-scope", "____test"))
        .await
}

fn replay_attempt() -> Option<ReplayAttempt> {
    Some(ReplayAttempt {
        attempt: 1,
        requested_by: "replacement-worker".into(),
    })
}

/// The tool server reads the row the worker journaled before dispatch. A
/// read that missed it sent the server to create the row itself, which the
/// existing row refused, and the call failed before the tool ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_journaled_call_runs_while_replicas_lag() -> Result<()> {
    let (mut harness, journal) = serve(TestToolset::default()).await?;
    let request = common::request("lagging-parent", "worker-call");
    journal_call(&journal, &request).await?;

    let reply = harness.call_tool(&request).await?;

    assert_eq!(reply.result, Ok(json!({"value": 42})));
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 1);
    assert_eq!(
        journal.get(&request).await?.context("journal row")?.reply,
        Some(reply)
    );
    harness.shutdown().await;
    Ok(())
}

/// A replay of a call whose reply is already journaled gets that reply. A
/// server that missed it would run the call again, or refuse it outright
/// for a tool that cannot replay, as this one cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_returns_the_journaled_reply_while_replicas_lag() -> Result<()> {
    let (mut harness, journal) = serve(TestToolset::default()).await?;
    let mut request = common::request("lagging-parent", "answered-call");
    journal_call(&journal, &request).await?;
    let saved = ToolReply {
        call_id: request.call_id.clone(),
        result: Ok(json!({"saved": true})),
        final_progress: None,
    };
    journal.complete(&request, saved.clone()).await?;
    request.replay = replay_attempt();

    assert_eq!(harness.call_tool(&request).await?, saved);
    assert_eq!(harness.toolset.echo_invocations.load(Ordering::SeqCst), 0);
    harness.shutdown().await;
    Ok(())
}

/// A replayed call is handed the job its first attempt checkpointed, so it
/// can resume that job instead of starting a second one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_resumes_the_journaled_checkpoint_while_replicas_lag() -> Result<()> {
    let mut toolset = TestToolset::default();
    toolset.idempotent = true;
    let (mut harness, journal) = serve(toolset).await?;
    let mut request = common::request("lagging-parent", "checkpointed-call");
    journal_call(&journal, &request).await?;
    journal
        .checkpoint_store(&request)
        .checkpoint(json!({"job": "first-attempt"}))
        .await?;
    request.replay = replay_attempt();

    let reply = harness.call_tool(&request).await?;

    assert_eq!(reply.result, Ok(json!({"value": 42})));
    let context = harness.toolset.last_context.lock().await.clone();
    assert_eq!(
        context.context("the tool ran")?.checkpoint,
        Some(json!({"job": "first-attempt"}))
    );
    harness.shutdown().await;
    Ok(())
}

/// What a tool reports it has produced lands in the row the worker journaled,
/// which is where a failed call's caller, wind-up and replay all look for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_result_lands_in_the_workers_row_while_replicas_lag() -> Result<()> {
    let (mut harness, journal) = serve(TestToolset::default()).await?;
    let mut request = common::request("lagging-parent", "partial-call");
    request.args = json!({"partial_result": {"pages": 3}, "error": "quota exhausted"});
    journal_call(&journal, &request).await?;

    let reply = harness.call_tool(&request).await?;

    assert_eq!(
        reply.result,
        Err(ToolErrorPayload::Recoverable("quota exhausted".into()))
    );
    let row = journal.get(&request).await?.context("journal row")?;
    assert_eq!(row.partial_result, Some(json!({"pages": 3})));
    harness.shutdown().await;
    Ok(())
}

/// A replacement worker looks up an interrupted round's calls by round and
/// transcript id. A call it cannot find counts as never journaled, so the
/// worker would answer it with a placeholder or run it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_lookups_find_the_workers_rows_while_replicas_lag() -> Result<()> {
    let (_server, journal) = stale_replica_broker().await?;
    let first = common::request_in_round("lagging-parent", "first-call", 4);
    let mut second = common::request_in_round("lagging-parent", "second-call", 5);
    second.tool_call_id = Some("later-model-call".into());
    journal_call(&journal, &first).await?;
    journal_call(&journal, &second).await?;

    let found = journal.find("lagging-parent", 4, "model-call").await?;
    assert_eq!(found.context("round 4's row")?.request, first);
    let rows = journal.records_in_rounds("lagging-parent", &[5]).await?;
    let call_ids: Vec<_> = rows
        .iter()
        .map(|row| row.request.call_id.as_str())
        .collect();
    assert_eq!(call_ids, ["second-call"]);
    Ok(())
}

/// Deleting a session purges its rows and leaves a tombstone, so a worker
/// still holding the session's transcript can neither journal nor dispatch
/// another call for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_session_takes_no_new_calls_while_replicas_lag() -> Result<()> {
    let (_server, journal) = stale_replica_broker().await?;
    journal_call(
        &journal,
        &common::request_in_round("deleted-parent", "before-deletion", 2),
    )
    .await?;

    journal.purge_session("deleted-parent").await?;

    assert!(journal
        .records_for_session("deleted-parent")
        .await?
        .is_empty());
    // A purged row is gone, not a row whose content failed to decode.
    assert!(journal
        .recorded("deleted-parent", "before-deletion")
        .await?
        .is_none());
    let late = common::request("deleted-parent", "after-deletion");
    let error = journal_call(&journal, &late)
        .await
        .expect_err("a deleted session takes no new calls");
    assert!(error.to_string().contains("deleted"), "{error:#}");
    Ok(())
}
