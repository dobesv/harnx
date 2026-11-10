//! The journal bucket's stream settings, on a bucket this build creates and on
//! one an older build left behind, and the rows that expire on their own.
use crate::common::{self, TOKEN};
use anyhow::{Context, Result};
use async_nats::header::NATS_MESSAGE_TTL;
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use harnx_toolset::{PartialResultStore, ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::{InvocationJournal, BUCKET};
use serde_json::json;
use std::time::Duration;

async fn connect(server_url: &str) -> Result<jetstream::Context> {
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server_url)
        .await
        .context("connect to the test broker")?;
    Ok(jetstream::new(client))
}

/// The journal stream's configuration as the server reports it.
async fn journal_stream_config(js: &jetstream::Context) -> Result<stream::Config> {
    Ok(js
        .get_stream(format!("KV_{BUCKET}"))
        .await?
        .info()
        .await?
        .config
        .clone())
}

/// The bucket as builds before compression and per-message TTLs created it.
async fn create_older_bucket(js: &jetstream::Context) -> Result<kv::Store> {
    let older = js
        .create_key_value(kv::Config {
            bucket: BUCKET.into(),
            num_replicas: 1,
            ..Default::default()
        })
        .await
        .context("create the bucket the way older builds did")?;
    let config = journal_stream_config(js).await?;
    assert!(
        config.compression != Some(stream::Compression::S2) && !config.allow_message_ttl,
        "the older bucket must start without either setting for this test to mean anything"
    );
    Ok(older)
}

/// What this build sets on the journal's stream: S2 compression, and
/// per-message TTLs with the expiry markers a KV config turns on with them.
fn assert_current_settings(config: &stream::Config) {
    assert_eq!(config.compression, Some(stream::Compression::S2));
    assert!(config.allow_message_ttl);
    assert_eq!(
        config.subject_delete_marker_ttl,
        Some(Duration::from_secs(60))
    );
}

/// The `Nats-TTL` header the latest revision of `key` was written with.
async fn ttl_of(js: &jetstream::Context, key: &str) -> Result<Option<String>> {
    let message = js
        .get_stream(format!("KV_{BUCKET}"))
        .await?
        .get_last_raw_message_by_subject(&format!("$KV.{BUCKET}.{key}"))
        .await
        .with_context(|| format!("read the latest revision of {key}"))?;
    Ok(message
        .headers
        .get(NATS_MESSAGE_TTL)
        .map(|ttl| ttl.as_str().to_owned()))
}

/// How many messages `subscriber` has queued. Call it after a round trip on
/// the subscriber's connection, so every message sent before it has arrived.
async fn received(subscriber: &mut async_nats::Subscriber) -> usize {
    let mut count = 0;
    while let Ok(Some(_)) =
        tokio::time::timeout(Duration::from_millis(100), subscriber.next()).await
    {
        count += 1;
    }
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_journal_bucket_has_this_builds_settings() -> Result<()> {
    let (server, _journal) = common::journal().await;
    let js = connect(&server.url).await?;

    assert_current_settings(&journal_stream_config(&js).await?);
    Ok(())
}

/// Builds before compression and per-message TTLs created the bucket without
/// them. Opening the journal turns both on and keeps the rows already there.
/// From then on the bucket matches what opening it would create, so opening it
/// again neither reads nor updates the stream's config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_an_older_journal_bucket_brings_it_up_to_date() -> Result<()> {
    let server = common::spawn_nats_server()
        .await?
        .expect("nats-server binary is required for journal tests");
    let js = connect(&server.url).await?;
    let older = create_older_bucket(&js).await?;
    older.put("standalone/kept", "row".into()).await?;

    InvocationJournal::ensure(&js, 1).await?;

    let config = journal_stream_config(&js).await?;
    assert_current_settings(&config);
    assert_eq!(config.num_replicas, 1);
    assert_eq!(older.get("standalone/kept").await?, Some("row".into()));

    let observer = connect(&server.url).await?;
    let mut infos = observer
        .client()
        .subscribe(format!("$JS.API.STREAM.INFO.KV_{BUCKET}"))
        .await?;
    let mut updates = observer
        .client()
        .subscribe(format!("$JS.API.STREAM.UPDATE.KV_{BUCKET}"))
        .await?;
    common::round_trip(&observer.client()).await?;
    InvocationJournal::ensure(&js, 1).await?;
    common::round_trip(&observer.client()).await?;
    assert_eq!(received(&mut infos).await, 0, "STREAM.INFO requests");
    assert_eq!(received(&mut updates).await, 0, "STREAM.UPDATE requests");
    Ok(())
}

/// A deleted session's tombstone expires after an hour, so a session started
/// again under the same agent and name can journal its calls after that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_sessions_tombstone_expires_after_an_hour() -> Result<()> {
    let (server, journal) = common::journal().await;
    let js = connect(&server.url).await?;

    journal.purge_session("gone").await?;

    assert_eq!(ttl_of(&js, "deleted/gone").await?.as_deref(), Some("3600"));
    Ok(())
}

/// A bucket no tool server from this build has opened yet rejects a message
/// that carries a TTL, and deleting a session must still work against it. The
/// tombstone goes in without one, and still refuses the session's late calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_session_in_a_bucket_without_ttls_still_leaves_a_tombstone() -> Result<()> {
    let server = common::spawn_nats_server()
        .await?
        .expect("nats-server binary is required for journal tests");
    let js = connect(&server.url).await?;
    let journal = InvocationJournal::from_store(&js, create_older_bucket(&js).await?);

    journal.purge_session("gone").await?;

    assert_eq!(ttl_of(&js, "deleted/gone").await?, None);
    let error = journal
        .record(&common::request("gone", "late"), ("echo", "scope", "srv"))
        .await
        .expect_err("a deleted session takes no new calls");
    assert!(error.to_string().contains("deleted"), "{error:#}");
    Ok(())
}

/// A call made outside any session has no session to be deleted with, so each
/// write of its row sets a week's TTL again: the first record, a partial
/// result, and the reply. A session's rows carry none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_rows_expire_a_week_after_their_last_write() -> Result<()> {
    let (server, journal) = common::journal().await;
    let js = connect(&server.url).await?;
    let week = Some("604800");
    let tool = ("echo", "scope", "srv");
    let reply = |call_id: &str| ToolReply {
        call_id: call_id.into(),
        result: Ok(json!({"value": 42})),
        final_progress: None,
    };

    let standalone = ToolRequest {
        parent_session_id: None,
        tool_call_id: None,
        ..common::request("unused", "direct")
    };
    journal.record(&standalone, tool).await?;
    assert_eq!(ttl_of(&js, "standalone/direct").await?.as_deref(), week);
    journal
        .partial_result_store(&standalone)
        .record_partial_result(json!({"step": 1}))
        .await?;
    assert_eq!(ttl_of(&js, "standalone/direct").await?.as_deref(), week);
    journal.complete(&standalone, reply("direct")).await?;
    assert_eq!(ttl_of(&js, "standalone/direct").await?.as_deref(), week);
    let row = journal.get(&standalone).await?.context("standalone row")?;
    assert!(row.reply.is_some() && row.partial_result.is_some());

    let in_session = common::request_in_round("parent", "in-session", 3);
    journal.record(&in_session, tool).await?;
    journal.complete(&in_session, reply("in-session")).await?;
    assert_eq!(
        ttl_of(&js, "sessions.parent.3.model-call.in-session").await?,
        None
    );
    Ok(())
}
