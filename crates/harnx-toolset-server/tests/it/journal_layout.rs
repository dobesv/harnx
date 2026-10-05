//! Journal rows are keyed `sessions.<session>.<round>.<tool call>.<call>`,
//! with each part one subject token, so a lookup lists just the rows it needs
//! instead of every session's.
use crate::common::{self, request_headers, TestHarness};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_toolset::{ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::{self, InvocationJournal, RecordedInvocation};
use serde_json::json;
use std::future::Future;
use std::time::Duration;

/// The subject filter of each listing the journal asks the stream leader for
/// while `operation` runs, together with what `operation` returned.
async fn listings_during<T>(
    server_url: &str,
    operation: impl Future<Output = Result<T>>,
) -> Result<(T, Vec<String>)> {
    let client = async_nats::ConnectOptions::new()
        .token(common::TOKEN.to_string())
        .connect(server_url)
        .await?;
    let mut requests = client
        .subscribe(format!(
            "$JS.API.STREAM.INFO.KV_{}",
            invocation_journal::BUCKET
        ))
        .await?;
    common::round_trip(&client).await?;
    let output = operation.await?;
    common::round_trip(&client).await?;
    let mut filters = Vec::new();
    while let Ok(Some(request)) =
        tokio::time::timeout(Duration::from_millis(100), requests.next()).await
    {
        let body: serde_json::Value = serde_json::from_slice(&request.payload)?;
        filters.push(body["subjects_filter"].as_str().unwrap_or("").to_owned());
    }
    filters.sort();
    Ok((output, filters))
}

/// Journal one row per `(session, call id, round)` the way a worker does.
async fn journal_rows(journal: &InvocationJournal, rows: &[(&str, &str, u64)]) -> Result<()> {
    for (session, call_id, round) in rows {
        journal
            .record(
                &common::request_in_round(session, call_id, *round),
                ("echo", "scope", "srv"),
            )
            .await?;
    }
    Ok(())
}

/// Replay finds one transcript call's row. Listing any wider than that call
/// in its round is what made every replay pay for every session's rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finding_a_call_lists_only_that_call_in_its_round() -> Result<()> {
    let (server, journal) = common::journal().await;
    journal_rows(
        &journal,
        &[
            ("parent", "asked", 4),
            ("parent", "later", 5),
            ("other-parent", "elsewhere", 4),
        ],
    )
    .await?;

    let (found, filters) =
        listings_during(&server.url, journal.find("parent", 4, "model-call")).await?;

    assert_eq!(
        found.map(|row| row.request.call_id),
        Some("asked".to_string())
    );
    assert_eq!(
        filters,
        ["$KV.harnx_tool_invocations.sessions.parent.4.model-call.*"]
    );
    Ok(())
}

/// Wind-up needs the rows of the rounds a `Cancel` interrupted, and nothing
/// from the session's other rounds or from other sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wind_up_lists_only_the_rounds_it_asks_for() -> Result<()> {
    let (server, journal) = common::journal().await;
    journal_rows(
        &journal,
        &[
            ("parent", "in-four", 4),
            ("parent", "in-five", 5),
            ("parent", "in-six", 6),
            ("other-parent", "elsewhere", 4),
        ],
    )
    .await?;

    let (rows, filters) =
        listings_during(&server.url, journal.records_in_rounds("parent", &[4, 6])).await?;

    let mut call_ids: Vec<_> = rows.into_iter().map(|row| row.request.call_id).collect();
    call_ids.sort();
    assert_eq!(call_ids, ["in-four", "in-six"]);
    assert_eq!(
        filters,
        [
            "$KV.harnx_tool_invocations.sessions.parent.4.>",
            "$KV.harnx_tool_invocations.sessions.parent.6.>",
        ]
    );
    Ok(())
}

/// Deleting a session lists that session's rows, plus the single-token keys
/// where rows written before this layout still live. Rows the current layout
/// wrote for other sessions are in neither listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_session_lists_its_own_rows_and_old_layout_keys() -> Result<()> {
    let (server, journal) = common::journal().await;
    journal_rows(
        &journal,
        &[("parent", "mine", 4), ("other-parent", "theirs", 4)],
    )
    .await?;

    let ((), filters) = listings_during(&server.url, journal.purge_session("parent")).await?;

    assert_eq!(
        filters,
        [
            "$KV.harnx_tool_invocations.*",
            "$KV.harnx_tool_invocations.sessions.parent.>",
        ]
    );
    assert!(journal.records_for_session("parent").await?.is_empty());
    assert_eq!(journal.records_for_session("other-parent").await?.len(), 1);
    Ok(())
}

/// Each part of a key is escaped into one subject token. These ids pair up
/// so that an escape letting `=` through unchanged gives both of a pair the
/// same token: two calls would then share a key, and two sessions each
/// other's rows. An id with a `.` in it must not span tokens either, or its
/// row drops out of the lookup for its round, and an empty id still has to
/// make a key the server accepts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ids_with_separators_or_escapes_keep_their_own_rows() -> Result<()> {
    let (_server, journal) = common::journal().await;
    let rows = [
        ("a.b", "call.0", "functions.bash_exec:0"),
        ("a.b", "call=2E0", "functions=2Ebash_exec=3A0"),
        ("a=2Eb", "call.1", ""),
    ];
    for (session, call_id, tool_call_id) in rows {
        let request = ToolRequest {
            tool_call_id: Some(tool_call_id.to_string()),
            ..common::request_in_round(session, call_id, 3)
        };
        journal.record(&request, ("echo", "scope", "srv")).await?;
    }

    for (session, call_id, tool_call_id) in rows {
        let row = journal
            .find(session, 3, tool_call_id)
            .await?
            .with_context(|| format!("no row for {tool_call_id:?} in {session}"))?;
        assert_eq!(row.request.call_id, call_id);
        let by_call = journal
            .recorded(session, call_id)
            .await?
            .with_context(|| format!("no row for {call_id} in {session}"))?;
        assert_eq!(by_call.request.tool_call_id.as_deref(), Some(tool_call_id));
    }
    for (session, expected) in [
        ("a.b", vec!["call.0", "call=2E0"]),
        ("a=2Eb", vec!["call.1"]),
    ] {
        let mut call_ids: Vec<_> = journal
            .records_for_session(session)
            .await?
            .into_iter()
            .map(|row| row.request.call_id)
            .collect();
        call_ids.sort();
        assert_eq!(call_ids, expected, "rows listed for {session}");
    }
    Ok(())
}

/// Rows written before this layout sit under `sessions/<session>/<call>`, and
/// nothing reads them any more. A session that has some resumes as if those
/// calls were never journaled rather than failing, and deleting the session
/// still deletes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_in_the_old_layout_are_ignored_and_deleted_with_their_session() -> Result<()> {
    let (server, journal) = common::journal().await;
    let request = common::request_in_round("parent", "old-call", 4);
    let row = RecordedInvocation {
        request,
        tool_name: "echo".into(),
        server: "srv".into(),
        server_scope: "scope".into(),
        tool_round: 4,
        started_at_ms: 0,
        reply: None,
        checkpoint: None,
        partial_result: None,
    };
    let client = async_nats::ConnectOptions::new()
        .token(common::TOKEN.to_string())
        .connect(&server.url)
        .await?;
    let store = async_nats::jetstream::new(client)
        .get_key_value(invocation_journal::BUCKET)
        .await?;
    store
        .put("sessions/parent/old-call", serde_json::to_vec(&row)?.into())
        .await?;

    assert!(journal.find("parent", 4, "model-call").await?.is_none());
    assert!(journal.records_in_rounds("parent", &[4]).await?.is_empty());
    assert!(journal.recorded("parent", "old-call").await?.is_none());

    journal.purge_session("parent").await?;
    assert!(
        store.get("sessions/parent/old-call").await?.is_none(),
        "deleting the session deletes its old rows"
    );
    Ok(())
}

/// A call with no parent session is journaled under `standalone/<call>`, and
/// the checkpoint its tool records belongs in that same row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standalone_call_records_its_checkpoint_in_its_own_row() -> Result<()> {
    let harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    common::wait_for_registration(&harness.client, &harness.instance_id).await?;
    let request = ToolRequest {
        parent_session_id: None,
        args: json!({"checkpoint": {"remote_job": "j-1"}}),
        ..common::request("unused", "standalone-call")
    };

    let message = harness
        .client
        .request_with_headers(
            harness.echo_subject(),
            request_headers(&request.call_id, &request.call_id),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;

    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert_eq!(reply.result, Ok(request.args.clone()));
    let journal =
        InvocationJournal::ensure(&async_nats::jetstream::new(harness.client.clone()), 1).await?;
    let row = journal.get(&request).await?.context("journal row")?;
    assert_eq!(row.checkpoint, Some(json!({"remote_job": "j-1"})));
    Ok(())
}
