//! Backend browse-back contract through real HTTP dispatch and JetStream.
mod support;

use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use support::*;

#[tokio::test]
async fn two_compactions_page_every_archived_message_once_and_keep_tool_rounds_atomic() -> Result<()>
{
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let expected = f.walk(15, 100).await?;
    for limit in [1, 2, 3, 4] {
        assert_eq!(f.walk(15, limit).await?, expected);
    }
    assert_archived_contents(&expected);
    assert_eq!(
        contents(&f.walk(5, 1).await?),
        [
            "Inspect the session log format.",
            "Tool calls and results are separate log entries."
        ]
    );
    Ok(())
}

fn assert_archived_contents(rows: &[Value]) {
    assert_eq!(
        contents(rows),
        [
            "The session log stores tool calls and results separately. No messages were kept.",
            "Implement the shared transcript view and verify paging.",
            "Inspect the archived middle of the turn.",
            "{\"text\":\"Compress archives the previous window; kept messages are re-logged.\"}",
            "Check the transcript view tests.",
            "{\"exit_code\":0,\"stdout\":\"Transcript view tests passed.\"}",
            "The shared view is implemented. Paging tests passed."
        ]
    );
    let ids: std::collections::HashSet<_> =
        rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), rows.len());
}

#[tokio::test]
async fn cursors_move_both_directions_and_http_returns_the_same_page() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let first = f.history(json!({"before_compaction":15,"limit":2})).await?;
    let second = f.history(json!({"cursor":first["next_cursor"]})).await?;
    let previous = f.history(json!({"cursor":second["prev_cursor"]})).await?;
    assert_eq!(first, previous);
    assert_eq!(first["prev_cursor"], Value::Null);
    assert_terminal_cursor_roundtrip(&f, &second).await?;
    assert_page_marker(&first);
    assert_http_page(&f, &first).await?;
    Ok(())
}

fn assert_page_marker(first: &Value) {
    assert_eq!(
        first["history_snapshot"][0]["id"],
        first["compaction"]["message_id"]
    );
    assert_eq!(
        first["compaction"],
        json!({
            "message_id":"a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3", "compaction_seq":5,
            "summary":"The session log stores tool calls and results separately. No messages were kept.",
            "from_seq":1, "to_seq":4, "kept_count":0
        })
    );
    assert_eq!(first["earlier_compaction"], 5);
}

async fn assert_http_page(f: &Fixture, expected: &Value) -> Result<()> {
    let response = f
        .request(
            Method::GET,
            "history/history?before_compaction=15&limit=2",
            "alice",
        )
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(response.json::<Value>().await?, *expected);
    let response = f
        .request(
            Method::GET,
            &format!(
                "history/history?cursor={}",
                expected["next_cursor"].as_str().unwrap()
            ),
            "alice",
        )
        .send()
        .await?;
    assert_rounds(&response.json::<Value>().await?);
    Ok(())
}

#[tokio::test]
async fn inclusive_ranges_select_whole_rounds_and_reject_bad_sequences() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    for seq in [8, 9] {
        let page = f
            .history(json!({"from_seq":seq,"to_seq":seq,"limit":1}))
            .await?;
        assert_rounds(&page);
        assert_eq!(page["history_snapshot"].as_array().unwrap().len(), 2);
    }
    assert_invalid_params(&f).await?;
    Ok(())
}

async fn assert_invalid_params(f: &Fixture) -> Result<()> {
    for params in [
        json!({"before_compaction":999}),
        json!({"before_compaction":7}),
        json!({"from_seq":0,"to_seq":1}),
        json!({"from_seq":1,"to_seq":24}),
        json!({"from_seq":1,"to_seq":16}),
        json!({"from_seq":10,"to_seq":9}),
        json!({"before_compaction":15,"limit":0}),
        json!({"before_compaction":15,"limit":1001}),
        json!({"cursor":"invalid!"}),
        json!({"unknown":1}),
        json!({}),
    ] {
        let response = f.rpc("session/history", params).await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = response.json().await?;
        assert_eq!(body["error"]["code"], -32602);
        assert!(!body["error"]["data"]["detail"].as_str().unwrap().is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn history_reads_leave_log_and_session_metadata_unchanged() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let before = f.store.get(&storage_key("history")).await?.unwrap();
    let log = f.log.load_events_async().await?;
    f.walk(15, 1).await?;
    f.walk(5, 1).await?;
    let after = f.store.get(&storage_key("history")).await?.unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(json!(after.metadata), json!(before.metadata));
    assert_eq!(f.log.load_events_async().await?, log);
    Ok(())
}

#[tokio::test]
async fn frozen_cursor_survives_later_rewind_and_cache_eviction() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let first = f
        .history(json!({"from_seq":15,"to_seq":23,"limit":2}))
        .await?;
    let params = json!({"cursor":first["next_cursor"]});
    let expected = f.history(params.clone()).await?;
    f.log
        .append_event_async(&SessionLogEntry::Rewind { after_seq: 4 })
        .await?;
    // More than the bounded cache's 16 distinct windows forces a stateless rebuild.
    for seq in 1..=22 {
        f.history(json!({"from_seq":seq,"to_seq":seq})).await?;
    }
    assert_eq!(f.history(params).await?, expected);
    Ok(())
}

#[tokio::test]
async fn cursors_cannot_be_reused_on_another_session_or_with_changed_limit() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let first = f.history(json!({"before_compaction":15,"limit":1})).await?;
    let params = json!({"cursor":first["next_cursor"]});
    let response = f
        .request(Method::POST, "other", "alice")
        .json(&json!({"jsonrpc":"2.0","id":7,"method":"session/history","params":params}))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.json::<Value>().await?["error"]["code"], -32602);
    let response = f
        .rpc(
            "session/history",
            json!({"cursor":first["next_cursor"],"limit":2}),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn auth_and_ownership_match_session_get_even_for_cached_pages() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let first = f.history(json!({"before_compaction":15,"limit":1})).await?;
    for user in ["bob", "stranger", ""] {
        assert_hidden(&f, user, &first["next_cursor"]).await?;
    }
    let response = f.request(Method::POST, "history", "admin").json(&json!({"jsonrpc":"2.0","id":1,"method":"session/history","params":{"cursor":first["next_cursor"]}})).send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

async fn assert_hidden(f: &Fixture, user: &str, cursor: &Value) -> Result<()> {
    let expected = if user.is_empty() {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::NOT_FOUND
    };
    for method in ["session/get", "session/history"] {
        let response = f
            .request(Method::POST, "history", user)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":{"cursor":cursor}}))
            .send()
            .await?;
        assert_eq!(response.status(), expected);
    }
    let response = f
        .request(
            Method::GET,
            &format!("history/history?cursor={}", cursor.as_str().unwrap()),
            user,
        )
        .send()
        .await?;
    assert_eq!(response.status(), expected);
    Ok(())
}

#[tokio::test]
async fn rules_off_preserve_unknown_session_json_rpc_error_and_public_history() -> Result<()> {
    let Some(f) = fixture(false).await? else {
        return Ok(());
    };
    let response = f.request(Method::POST, "missing", "bob").json(&json!({"jsonrpc":"2.0","id":1,"method":"session/history","params":{"before_compaction":5}})).send().await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.json::<Value>().await?["error"]["code"], -32001);
    let response = f
        .request(Method::GET, "history/history?before_compaction=5", "bob")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn http_history_rejects_bad_queries_and_non_get_methods() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    for query in [
        "",
        "?limit=0&before_compaction=5",
        "?before_compaction=5&limit=1&limit=2",
        "?before_compaction=999",
        "?unknown=1",
    ] {
        let response = f
            .request(Method::GET, &format!("history/history{query}"), "alice")
            .send()
            .await?;
        assert_http_error(response).await?;
    }
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let response = f
            .request(method, "history/history?before_compaction=5", "alice")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
    Ok(())
}

#[tokio::test]
async fn active_window_ranges_keep_archive_link_without_inventing_a_marker() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let page = f.history(json!({"from_seq":16,"to_seq":20})).await?;
    assert_eq!(page["earlier_compaction"], 15);
    assert!(page.get("compaction").is_none());
    assert_eq!(page["history_snapshot"].as_array().unwrap().len(), 4);
    assert_rounds(&page);
    Ok(())
}

#[tokio::test]
async fn empty_reserved_session_reports_missing_compaction_or_out_of_range() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    for params in [
        json!({"before_compaction":5}),
        json!({"from_seq":1,"to_seq":1}),
    ] {
        let response = f
            .request(Method::POST, "other", "alice")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"session/history","params":params}))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = response.json().await?;
        assert_eq!(body["error"]["code"], -32602);
        assert!(!body["error"]["data"]["detail"]
            .as_str()
            .unwrap()
            .contains("cursor"));
    }
    Ok(())
}

async fn assert_http_error(response: reqwest::Response) -> Result<()> {
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await?;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body.get("jsonrpc").is_none());
    Ok(())
}

#[tokio::test]
async fn concurrent_cursor_reads_return_identical_round_safe_pages() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let first = f.history(json!({"before_compaction":15,"limit":2})).await?;
    let params = json!({"cursor":first["next_cursor"]});
    let pages = futures::future::try_join_all((0..16).map(|_| f.history(params.clone()))).await?;
    for page in &pages {
        assert_eq!(page, &pages[0]);
        assert_rounds(page);
    }
    Ok(())
}

#[tokio::test]
async fn initial_ranges_after_rewind_do_not_serve_removed_archives() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    f.log
        .append_event_async(&SessionLogEntry::Rewind { after_seq: 1 })
        .await?;
    f.log.append_event_async(&entries()[20].1).await?;
    let current = f.history(json!({"from_seq":1,"to_seq":25})).await?;
    assert_eq!(
        contents(current["history_snapshot"].as_array().unwrap()),
        [
            "Inspect the session log format.",
            "Show the previous compaction's history."
        ]
    );
    for to_seq in [4, 14] {
        let subset = f.history(json!({"from_seq":1,"to_seq":to_seq})).await?;
        assert_eq!(
            contents(subset["history_snapshot"].as_array().unwrap()),
            ["Inspect the session log format."]
        );
    }
    assert_archived_contents(&f.walk(15, 100).await?);
    Ok(())
}

#[tokio::test]
async fn rest_history_missing_session_returns_not_found_error() -> Result<()> {
    let Some(f) = fixture(false).await? else {
        return Ok(());
    };
    let response = f
        .request(Method::GET, "missing/history?before_compaction=5", "bob")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(
        response.json::<Value>().await?,
        json!({
            "error": { "message":"session not found", "type":"invalid_request_error" }
        })
    );
    Ok(())
}

#[tokio::test]
async fn rest_history_unknown_query_names_have_clear_errors() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    for value in ["true", "1", ""] {
        let response = f
            .request(
                Method::GET,
                &format!("history/history?debug={value}"),
                "alice",
            )
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<Value>().await?,
            json!({
                "error": { "message":"unknown history parameter: debug", "type":"invalid_request_error" }
            })
        );
    }
    Ok(())
}

async fn assert_terminal_cursor_roundtrip(f: &Fixture, second: &Value) -> Result<()> {
    let terminal = f.history(json!({"cursor":second["next_cursor"]})).await?;
    let previous = f.history(json!({"cursor":terminal["prev_cursor"]})).await?;
    assert_eq!(terminal["next_cursor"], Value::Null);
    assert_eq!(previous, *second);
    Ok(())
}
