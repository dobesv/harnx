//! Physical bounded-reader tests (plan #1550 Task 4a): real JetStream, real
//! caps. The record-cap test deliberately writes 1001 entries so the cap is
//! exercised against the broker, not the in-memory simulation.

use super::*;
use crate::nats_test_common::spawn_nats_server;
use harnx_core::message::{MessageContent, MessageRole};

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_window_returns_sorted_entries_with_source_index() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new_with_replicas(js, "window-sorted", 1);
    for i in 0..5 {
        log.append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let window = log.load_window_async(None, None).await.unwrap();
    let seqs: Vec<u64> = window.entries.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    assert_eq!(window.scanned_through, Some(5));
    assert!(!window.capped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_window_respects_inclusive_index_range() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new_with_replicas(js, "window-range", 1);
    for i in 0..6 {
        log.append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let window = log.load_window_async(Some(2), Some(4)).await.unwrap();
    let seqs: Vec<u64> = window.entries.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, vec![2, 3, 4]);
    assert_eq!(window.scanned_through, Some(4));
    assert!(!window.capped);

    // Ranges beyond the tail are empty and not capped.
    let window = log.load_window_async(Some(7), None).await.unwrap();
    assert!(window.entries.is_empty());
    assert_eq!(window.scanned_through, None);
    assert!(!window.capped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_window_missing_stream_is_empty_not_capped() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new(js, "window-never-created");
    let window = log.load_window_async(None, None).await.unwrap();
    assert!(window.entries.is_empty());
    assert_eq!(window.scanned_through, None);
    assert!(!window.capped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_window_caps_records_at_a_thousand() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new_with_replicas(js, "window-record-cap", 1);
    // One more than the cap, so the first page must stop early.
    for i in 0..=SCAN_RECORD_LIMIT as u64 {
        log.append_event_async(&user(&format!("filler {i}")))
            .await
            .unwrap();
    }
    let window = log.load_window_async(None, None).await.unwrap();
    assert_eq!(window.entries.len(), SCAN_RECORD_LIMIT);
    assert_eq!(window.scanned_through, Some(SCAN_RECORD_LIMIT as u64));
    assert!(window.capped, "log continues past the record cap");

    // The next page resumes exactly at the cap boundary: no gap, no dup.
    let resume = window.scanned_through.unwrap() + 1;
    let window = log.load_window_async(Some(resume), None).await.unwrap();
    let seqs: Vec<u64> = window.entries.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, vec![resume]);
    assert_eq!(window.scanned_through, Some(resume));
    assert!(!window.capped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_window_caps_scanned_bytes_at_eight_mib() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new_with_replicas(js, "window-byte-cap", 1);
    // 17 x ~512 KiB entries: ~8.4 MiB total, past the 8 MiB byte cap but
    // nowhere near the record cap, so the byte cap is what stops the scan.
    // Payloads stay below the broker's 1 MiB max payload.
    let total = 17u64;
    let body = "k".repeat(512 * 1024);
    for i in 0..total {
        log.append_event_async(&user(&format!("{body} entry {i}")))
            .await
            .unwrap();
    }
    let window = log.load_window_async(None, None).await.unwrap();
    assert!(
        window.entries.len() < total as usize,
        "byte cap must stop before the log ends"
    );
    assert!(window.capped);
    let scanned = window.scanned_through.unwrap();
    assert_eq!(window.entries.last().unwrap().0, scanned);
    assert!(window.entries.len() >= 15, "cap is ~8 MiB / 512 KiB");

    // Resume covers the remainder exactly.
    let window = log
        .load_window_async(Some(scanned + 1), None)
        .await
        .unwrap();
    let seqs: Vec<u64> = window.entries.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, (scanned + 1..=total).collect::<Vec<u64>>());
    assert!(!window.capped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_entry_fetches_one_durable_sequence() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new_with_replicas(js, "window-entry-lookup", 1);
    for i in 0..3 {
        log.append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let entry = log.load_entry_async(2).await.unwrap();
    assert!(matches!(entry, Some(SessionLogEntry::Message { .. })));
    assert_eq!(log.load_entry_async(999).await.unwrap(), None);

    // Missing stream reads as absent, not as an error.
    let missing = NatsSessionLog::new(
        async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap()),
        "window-never-created",
    );
    assert_eq!(missing.load_entry_async(1).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_window_skips_purged_sequences_and_keeps_progress() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new_with_replicas(js.clone(), "window-gaps", 1);
    for i in 0..5 {
        log.append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let stream = js
        .get_stream(stream_name_for_session("window-gaps"))
        .await
        .unwrap();
    // Purge 2..=5, keeping only the head entry: the scan must still report
    // examining the purged tail.
    for seq in 2..=5 {
        assert!(stream.delete_message(seq).await.unwrap());
    }
    let window = log.load_window_async(None, None).await.unwrap();
    let indexes: Vec<u64> = window.entries.iter().map(|(index, _)| *index).collect();
    assert_eq!(indexes, vec![1], "purged sequences are skipped");
    assert_eq!(window.scanned_through, Some(5));
    assert!(!window.capped, "the full requested range was scanned");
    // A range whose every sequence was purged still reports its progress —
    // recomputing from entries would imply nothing was examined.
    let window = log.load_window_async(Some(2), Some(4)).await.unwrap();
    assert!(window.entries.is_empty());
    assert_eq!(window.scanned_through, Some(4));
    assert!(!window.capped);
    // Single-entry lookup of a purged sequence is a gap, not an error.
    assert_eq!(log.load_entry_async(3).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_reads_never_mutate_the_log() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    let log = NatsSessionLog::new_with_replicas(js, "window-pure-read", 1);
    for i in 0..4 {
        log.append_event_async(&user(&format!("entry {i}")))
            .await
            .unwrap();
    }
    let before = log.load_events_async().await.unwrap();
    log.load_window_async(None, None).await.unwrap();
    log.load_window_async(Some(2), Some(3)).await.unwrap();
    log.load_entry_async(2).await.unwrap();
    log.load_entry_async(999).await.unwrap();
    let after = log.load_events_async().await.unwrap();
    assert_eq!(before, after, "bounded reads must be side-effect free");
}
