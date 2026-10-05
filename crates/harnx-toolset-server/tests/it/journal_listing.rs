//! The journal lists a session's keys from `STREAM.INFO`'s subject index.
//! While a replicated stream has no leader, every replica answers that request
//! from its own store, however far behind it is, and the answer names no
//! leader.
use crate::common;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_toolset_server::invocation_journal::{self, InvocationJournal};
use std::time::Duration;

const LEADERLESS_INFO_SUBJECT: &str = "harnx.test.leaderless-stream-info";

/// Shaped like a nats-server 2.11.6 replica's answer after the leader and
/// the other follower of its three-replica stream were killed, with the
/// names and subjects changed to the journal's. It lists none of the
/// session's rows.
const LEADERLESS_STREAM_INFO: &str = r#"{
  "type": "io.nats.jetstream.api.v1.stream_info_response",
  "total": 1, "offset": 0, "limit": 100000,
  "config": {
    "name": "KV_harnx_tool_invocations",
    "subjects": ["$KV.harnx_tool_invocations.>"],
    "retention": "limits", "max_consumers": -1, "max_msgs": -1, "max_bytes": -1,
    "max_age": 0, "max_msgs_per_subject": 1, "max_msg_size": -1, "discard": "new",
    "storage": "file", "num_replicas": 3, "duplicate_window": 120000000000,
    "compression": "none", "allow_direct": true, "mirror_direct": false,
    "sealed": false, "deny_delete": true, "deny_purge": false,
    "allow_rollup_hdrs": true, "consumer_limits": {}, "allow_msg_ttl": false
  },
  "created": "2026-10-02T23:03:21.72533572Z",
  "state": {
    "messages": 1, "bytes": 84, "first_seq": 1,
    "first_ts": "2026-10-02T23:03:21.795235327Z", "last_seq": 1,
    "last_ts": "2026-10-02T23:03:21.852302061Z", "num_subjects": 1,
    "subjects": {"$KV.harnx_tool_invocations.sessions/earlier-session/earlier-call": 1},
    "consumer_count": 0
  },
  "cluster": {
    "name": "harnx", "raft_group": "S-R3F-YdCvy8pQ",
    "replicas": [
      {"name": "nats-1", "current": false, "active": 15139942995, "lag": 3, "peer": "k2My6qdB"},
      {"name": "nats-2", "current": false, "active": 0, "lag": 3, "peer": "noC8kOtg"}
    ]
  },
  "ts": "2026-10-02T23:03:36.867983832Z"
}"#;

/// A listing from a replica that names no leader may be missing rows, and a
/// lookup that trusted it would report a journaled call as never made. The
/// broker hands the journal stream's `STREAM.INFO` to a responder that
/// answers the way such a replica does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_without_a_stream_leader_is_refused() -> Result<()> {
    let config = format!(
        r#"mappings = {{
  "$JS.API.STREAM.INFO.KV_{}": "{LEADERLESS_INFO_SUBJECT}"
}}
"#,
        invocation_journal::BUCKET
    );
    let server = common::spawn_configured_nats_server(Some(&config))
        .await?
        .context("nats-server required")?;
    let client = async_nats::ConnectOptions::new()
        .token(common::TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let mut requests = client.subscribe(LEADERLESS_INFO_SUBJECT).await?;
    let responder = client.clone();
    tokio::spawn(async move {
        while let Some(request) = requests.next().await {
            if let Some(reply) = request.reply {
                let _ = responder
                    .publish(reply, LEADERLESS_STREAM_INFO.into())
                    .await;
            }
        }
    });
    client.flush().await?;
    let journal = InvocationJournal::ensure(&async_nats::jetstream::new(client), 1).await?;
    let request = common::request_in_round("lagging-parent", "journaled-call", 4);
    journal
        .record(&request, ("test_echo", "worker-scope", "____test"))
        .await?;

    let found = journal.find("lagging-parent", 4, "model-call").await;

    assert!(found.is_err(), "the lookup trusted the listing: {found:?}");
    Ok(())
}

/// The leader rebuilds the stream's whole subject index for every listing
/// request, including one for the page past the end, so a listing that fits
/// in one page asks for it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_that_fits_one_page_asks_for_it_once() -> Result<()> {
    let (server, journal) = common::journal().await;
    for call_id in ["first-call", "second-call"] {
        journal
            .record(
                &common::request("listed-parent", call_id),
                ("test_echo", "worker-scope", "____test"),
            )
            .await?;
    }
    let client = async_nats::ConnectOptions::new()
        .token(common::TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let mut requests = client
        .subscribe(format!(
            "$JS.API.STREAM.INFO.KV_{}",
            invocation_journal::BUCKET
        ))
        .await?;
    common::round_trip(&client).await?;

    let rows = journal.records_for_session("listed-parent").await?;

    assert_eq!(rows.len(), 2);
    common::round_trip(&client).await?;
    let mut seen = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), requests.next()).await
    {
        seen += 1;
    }
    assert_eq!(seen, 1, "listing requests sent");
    Ok(())
}
