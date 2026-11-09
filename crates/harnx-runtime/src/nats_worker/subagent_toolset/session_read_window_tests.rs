//! Task 4a seam tests: the shared backend (`read_session_log_window`) over a
//! real JetStream log — sorted bounded pages, physical-cap resume, sparse
//! filter progress, prompt-scoped ranges, continuation recovery, enclosing
//! metadata budget, and read-only behavior.

use super::*;
use crate::nats_session_log::{stream_name_for_session, NatsSessionLog};
use crate::nats_test_common::{spawn_nats_server, NatsServerHandle};
use harnx_core::message::{MessageContent, MessageRole};
use serde_json::json;

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn resolve(value: serde_json::Value) -> ResolvedRead {
    let options: ReadOptions = serde_json::from_value(value).expect("read options deserialize");
    options.resolve().expect("read options resolve")
}

fn serialized(value: &serde_json::Value) -> usize {
    serde_json::to_string(value)
        .expect("value serializes")
        .len()
}

async fn spawn_log(name: &str) -> Option<(NatsSessionLog, NatsServerHandle)> {
    let server = spawn_nats_server().await.unwrap()?;
    let url = server.url().to_owned();
    let js = async_nats::jetstream::new(async_nats::connect(url).await.unwrap());
    // The handle owns the server process; dropping it kills the broker, so it
    // must stay bound for the whole test.
    Some((NatsSessionLog::new_with_replicas(js, name, 1), server))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_returns_bounded_sorted_page_with_continuation_recovery() {
    let Some((log, _server)) = spawn_log("seam-bounded-page").await else {
        return;
    };
    for i in 0..3 {
        log.append_event_async(&user(&format!("plain entry {i}")))
            .await
            .unwrap();
    }
    // Giant multibyte entry forces preview/cap omissions under a small budget.
    log.append_event_async(&user(&"héllo 😀 →".repeat(20_000)))
        .await
        .unwrap();
    let last = log.append_event_async(&user("tail entry")).await.unwrap();

    let reserve = 300;
    let read = resolve(json!({ "max_output_bytes": 2048, "limit": 500 }));
    let page = read_session_log_window(&log, &read, reserve)
        .await
        .expect("page");
    assert!(
        serialized(&page) + reserve <= 2048,
        "enclosing result must respect the budget"
    );
    let events = page["events"].as_array().expect("events");
    // Sorted source indexes, contiguous, ending at the tail entry.
    let indexes: Vec<u64> = events
        .iter()
        .map(|event| event["index"].as_u64().expect("index"))
        .collect();
    assert_eq!(indexes.first().copied(), Some(1));
    assert_eq!(indexes.last().copied(), Some(last));
    for pair in indexes.windows(2) {
        assert_eq!(pair[1] - pair[0], 1, "delivered rows must be contiguous");
    }
    assert_eq!(page["scanned_through"], json!(last));
    assert_eq!(
        page["truncated"],
        json!(true),
        "giant entry omissions report truncation"
    );
    let ranges = page["omitted_ranges"].as_array().expect("omitted ranges");
    assert!(!ranges.is_empty());

    assert_omissions_recoverable(&log, &page).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_resumes_after_physical_byte_cap_without_gaps() {
    let Some((log, _server)) = spawn_log("seam-byte-cap-resume").await else {
        return;
    };
    // ~535 KiB per entry across many short lines, so head/tail previews stay
    // tiny while the serialized log (~9.6 MiB over 18 entries) runs past the
    // 8 MiB physical scan cap. Small previews matter: they let a full page
    // deliver every scanned event, so `next_index` reports the *scan* resume
    // point instead of a budget cut.
    let line = "k".repeat(100);
    let body = std::iter::repeat_n(line, 5_300)
        .collect::<Vec<_>>()
        .join("\n");
    let mut last = 0u64;
    for i in 0..18u64 {
        last = log
            .append_event_async(&user(&format!("{body} entry {i}")))
            .await
            .unwrap();
    }
    let read = resolve(json!({
        "max_output_bytes": 65_536,
        "limit": 500,
        "head_lines": 2,
        "tail_lines": 2,
    }));
    let page = read_session_log_window(&log, &read, 0).await.expect("page");
    assert!(serialized(&page) <= 65_536);
    // The page delivered every event the scan reached, so truncation comes
    // from the physical cap itself.
    let events = page["events"].as_array().expect("events");
    let last_delivered = events.last().expect("non-empty")["index"]
        .as_u64()
        .expect("index");
    let next = page["next_index"]
        .as_u64()
        .expect("physical cap reports resume");
    assert_eq!(
        next,
        last_delivered + 1,
        "resume follows the last delivered"
    );
    assert!(next < last, "resume points inside the capped log");

    // Second page resumes at `next` and reaches the tail exactly.
    let read = resolve(json!({
        "max_output_bytes": 65_536,
        "limit": 500,
        "index_min": next,
        "head_lines": 2,
        "tail_lines": 2,
    }));
    let page2 = read_session_log_window(&log, &read, 0)
        .await
        .expect("second page");
    assert!(serialized(&page2) <= 65_536);
    // Reaching the tail clears the *resume point* even though head/tail
    // previews keep the page marked truncated (recoverable per event).
    assert_eq!(page2["next_index"], json!(null));
    let events = page2["events"].as_array().expect("events");
    let first = events[0]["index"].as_u64().expect("index");
    let last_index = events.last().expect("non-empty")["index"]
        .as_u64()
        .expect("index");
    assert_eq!(first, next, "second page resumes where the cap stopped");
    assert_eq!(last_index, last, "second page reaches the tail");
    for pair in events.windows(2) {
        assert_eq!(
            pair[1]["index"].as_u64().unwrap() - pair[0]["index"].as_u64().unwrap(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_sparse_filter_and_missing_log_progress() {
    let Some((log, _server)) = spawn_log("seam-sparse-filter").await else {
        return;
    };
    let mut last = 0u64;
    for i in 0..5 {
        last = log
            .append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let sparse = resolve(json!({ "types": ["cancel"] }));
    let page = read_session_log_window(&log, &sparse, 0)
        .await
        .expect("page");
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["scanned_through"], json!(last));
    assert_eq!(page["next_index"], json!(null));
    assert_eq!(page["truncated"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_missing_log_reads_empty_without_error() {
    let Some((_log, server)) = spawn_log("seam-never-created").await else {
        return;
    };
    let missing = NatsSessionLog::new(
        async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap()),
        "seam-absent",
    );
    let page = read_session_log_window(&missing, &resolve(json!({})), 0)
        .await
        .expect("empty page");
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["scanned_through"], json!(null));
    assert_eq!(page["truncated"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_keeps_physical_progress_over_purged_ranges() {
    let Some((log, _server)) = spawn_log("seam-purged").await else {
        return;
    };
    for i in 0..6 {
        log.append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let stream = log
        .jetstream()
        .get_stream(stream_name_for_session("seam-purged"))
        .await
        .unwrap();
    // Purge a middle range (2..=4), keeping the head and tail entries.
    // Deleting the head would advance JetStream's first_sequence and move the
    // range outside the stream's own bounds; a middle purge is the shape a
    // bounded reader must survive.
    for seq in 2..=4 {
        assert!(stream.delete_message(seq).await.unwrap());
    }
    // The requested range is entirely purged: the page has no events but
    // must still report the scan examined through 4, or index_min paging
    // would stall below the real tail.
    let read = resolve(json!({ "index_min": 2, "index_max": 4 }));
    let page = read_session_log_window(&log, &read, 0)
        .await
        .expect("purged page");
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["scanned_through"], json!(4));
    assert_eq!(page["next_index"], json!(null));
    assert_eq!(page["truncated"], json!(false));
    // A sparse filter over the purged range keeps the same progress.
    let sparse = resolve(json!({ "index_min": 2, "index_max": 4, "types": ["cancel"] }));
    let page = read_session_log_window(&log, &sparse, 0)
        .await
        .expect("sparse purged page");
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["scanned_through"], json!(4));
    assert_eq!(page["next_index"], json!(null));
    // The surviving head and tail read normally.
    let read = resolve(json!({}));
    let page = read_session_log_window(&log, &read, 0).await.expect("page");
    let events = page["events"].as_array().expect("events");
    assert_eq!(
        events
            .iter()
            .map(|e| e["index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 5, 6]
    );
    assert_eq!(page["scanned_through"], json!(6));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_pins_effective_index_max_to_the_requested_range() {
    let Some((log, _server)) = spawn_log("seam-pinned-range").await else {
        return;
    };
    for i in 0..8 {
        log.append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let read = resolve(json!({ "index_max": 4 }));
    let page = read_session_log_window(&log, &read, 0).await.expect("page");
    let events = page["events"].as_array().expect("events");
    assert!(events
        .iter()
        .all(|event| event["index"].as_u64().unwrap() <= 4));
    assert_eq!(page["effective_index_max"], json!(4));
    assert_eq!(page["scanned_through"], json!(4));
    // Later appends cannot shift a pinned historical page.
    log.append_event_async(&user("later entry")).await.unwrap();
    let page = read_session_log_window(&log, &read, 0).await.expect("page");
    assert_eq!(page["effective_index_max"], json!(4));
    assert_eq!(page["scanned_through"], json!(4));
    assert_eq!(page["events"].as_array().expect("events").len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_continuation_missing_entry_fails_closed() {
    let Some((log, _server)) = spawn_log("seam-missing-entry").await else {
        return;
    };
    log.append_event_async(&user("only entry")).await.unwrap();
    let read = resolve(json!({ "entry_index": 999, "byte_offset": 0 }));
    let error = read_session_log_window(&log, &read, 0)
        .await
        .expect_err("absent entry must fail");
    assert_eq!(
        error,
        "read.entry_index 999 is not present in the session log"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seam_reads_do_not_mutate_the_log() {
    let Some((log, _server)) = spawn_log("seam-pure-read").await else {
        return;
    };
    for i in 0..4 {
        log.append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let before = log.load_events_async().await.unwrap();
    let read = resolve(json!({ "max_output_bytes": 1024 }));
    read_session_log_window(&log, &read, 50).await.unwrap();
    let sparse = resolve(json!({ "types": ["cancel"] }));
    read_session_log_window(&log, &sparse, 0).await.unwrap();
    let continuation = resolve(json!({ "entry_index": 2, "byte_offset": 0 }));
    read_session_log_window(&log, &continuation, 0)
        .await
        .unwrap();
    let after = log.load_events_async().await.unwrap();
    assert_eq!(before, after, "the seam must be a pure read");
}

async fn assert_omissions_recoverable(log: &NatsSessionLog, page: &Value) {
    // Every omitted interval is recoverable byte-for-byte via the seam's
    // continuation mode.
    let rendered = {
        let window = log.load_window_async(Some(4), Some(4)).await.unwrap();
        render_event_text(&window.entries[0].1)
    };
    for range in page["omitted_ranges"].as_array().expect("omitted ranges") {
        let entry_index = range["entry_index"].as_u64().expect("entry_index");
        let mut offset = range["start"].as_u64().expect("start");
        let end = range["end"].as_u64().expect("end");
        let mut recovered = String::new();
        while offset < end {
            let read = resolve(json!({
                "max_output_bytes": 1024,
                "entry_index": entry_index,
                "byte_offset": offset,
            }));
            let chunk = read_session_log_window(log, &read, 0).await.expect("chunk");
            assert!(serialized(&chunk) <= 1024);
            recovered.push_str(chunk["text"].as_str().expect("text"));
            match chunk["continuation"].as_object() {
                Some(next) => offset = next["byte_offset"].as_u64().expect("next offset"),
                None => break,
            }
        }
        let start = range["start"].as_u64().expect("start") as usize;
        let end = range["end"].as_u64().expect("end") as usize;
        assert_eq!(recovered, rendered[start..end]);
    }
}
