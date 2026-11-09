use super::*;

fn assert_progress(page: &Value, scanned: Option<u64>, next: Option<u64>, truncated: bool) {
    assert_eq!(
        (
            &page["scanned_through"],
            &page["next_index"],
            &page["truncated"]
        ),
        (&json!(scanned), &json!(next), &json!(truncated)),
    );
}

#[test]
fn sequential_page_delivers_complete_events() {
    let entries = vec![
        (1u64, message("first")),
        (2u64, message("second")),
        (3u64, message("third")),
    ];
    let read = resolve(json!({})).expect("defaults");
    let page = read_events(&entries, &read).expect("page");
    let events = page["events"].as_array().expect("events array");
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|event| event["complete"] == json!(true)));
    assert_progress(&page, Some(3), None, false);
    assert_eq!(page["effective_index_max"], json!(3));
    assert_eq!(page["continuation"], json!(null));
    assert_eq!(page["omitted_ranges"], json!([]));
    for (event, index) in events.iter().zip([1u64, 2, 3]) {
        assert_eq!(event["index"], json!(index));
        assert_eq!(event["type"], json!("message"));
    }
}

#[test]
fn empty_log_and_empty_filter_report_scan_progress() {
    let read = resolve(json!({})).expect("defaults");
    let page = read_events(&[], &read).expect("empty log page");
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["effective_index_max"], json!(null));
    assert_progress(&page, None, None, false);

    let entries = vec![(1u64, message("first")), (2u64, message("second"))];
    let sparse = resolve(json!({ "types": ["cancel"] })).expect("valid filter");
    let page = read_events(&entries, &sparse).expect("empty filtered page");
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["scanned_through"], json!(2));
    assert_eq!(page["truncated"], json!(false));
}

#[test]
fn filters_and_inclusive_index_range_apply() {
    let entries = vec![
        (1u64, message("alpha")),
        (2u64, tool_call_entry("bash_exec")),
        (3u64, message("beta with needle")),
        (4u64, message("gamma")),
    ];
    let by_type = resolve(json!({ "types": ["message"] })).expect("valid");
    let page = read_events(&entries, &by_type).expect("page");
    let indexes: Vec<u64> = page["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["index"].as_u64().expect("index"))
        .collect();
    assert_eq!(indexes, vec![1, 3, 4]);

    let by_tool = resolve(json!({ "tool_name": "bash_exec" })).expect("valid");
    let page = read_events(&entries, &by_tool).expect("page");
    assert_eq!(page["events"][0]["index"], json!(2));

    let by_regex = resolve(json!({ "text_regex": "needle" })).expect("valid");
    let page = read_events(&entries, &by_regex).expect("page");
    assert_eq!(page["events"].as_array().expect("events").len(), 1);
    assert_eq!(page["events"][0]["index"], json!(3));

    let by_range = resolve(json!({ "index_min": 2, "index_max": 3 })).expect("valid");
    let page = read_events(&entries, &by_range).expect("page");
    assert_eq!(page["effective_index_max"], json!(3));
    let indexes: Vec<u64> = page["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["index"].as_u64().expect("index"))
        .collect();
    assert_eq!(indexes, vec![2, 3]);
}

#[test]
fn limit_cuts_the_page_and_reports_next_index() {
    let entries: Vec<(u64, SessionLogEntry)> = (0..5)
        .map(|i| (i, message(&format!("entry {i}"))))
        .collect();
    let read = resolve(json!({ "limit": 2 })).expect("valid");
    let page = read_events(&entries, &read).expect("page");
    let events = page["events"].as_array().expect("events");
    assert_eq!(events.len(), 2);
    assert_eq!(page["truncated"], json!(true));
    assert_eq!(page["next_index"], json!(2));
}

#[test]
fn scan_record_cap_limits_the_window() {
    let entries: Vec<(u64, SessionLogEntry)> =
        (0..=1000u64).map(|i| (i, message("filler"))).collect();
    // The record cap stops the scan at the 1000th entry; the default limit
    // (100 matched events) is what cuts this page, so next_index points at
    // the first undelivered match.
    let read = resolve(json!({})).expect("defaults");
    let page = read_events(&entries, &read).expect("page");
    assert_progress(&page, Some(999), Some(100), true);
    assert_eq!(page["events"].as_array().expect("events").len(), 100);

    // A sparse filter that matches nothing still reports how far the capped
    // scan reached, so the next page can resume at the scan point.
    let sparse = resolve(json!({ "types": ["cancel"] })).expect("valid");
    let page = read_events(&entries, &sparse).expect("page");
    assert_eq!(page["events"], json!([]));
    assert_progress(&page, Some(999), Some(1000), true);

    // Resuming at the scan point delivers the capped-out tail exactly.
    let resumed = resolve(json!({ "index_min": 1000 })).expect("valid");
    let page = read_events(&entries, &resumed).expect("page");
    assert_eq!(page["events"][0]["index"], json!(1000));
    assert_progress(&page, Some(1000), None, false);
}

#[test]
fn sequential_read_is_deterministic() {
    let entries = vec![
        (1u64, message(&"z".repeat(5_000))),
        (2u64, message("plain")),
        (3u64, tool_call_entry("bash_exec")),
    ];
    let read = resolve(json!({ "max_output_bytes": 1024 })).expect("valid");
    let first = read_events(&entries, &read).expect("page");
    let second = read_events(&entries, &read).expect("page");
    assert_eq!(
        serde_json::to_string(&first).expect("serializes"),
        serde_json::to_string(&second).expect("serializes")
    );
}

#[test]
fn read_window_dispatches_by_mode() {
    let entries = vec![(1u64, message("hello"))];
    let sequential = resolve(json!({})).expect("defaults");
    let page = read_window(&entries, &sequential).expect("page");
    assert!(page["events"].as_array().expect("events").len() == 1);

    let continuation = resolve(json!({ "entry_index": 1, "byte_offset": 0 })).expect("valid");
    let chunk = read_window(&entries, &continuation).expect("chunk");
    assert_eq!(chunk["entry_index"], json!(1));
}

#[test]
fn physically_capped_windows_report_the_scan_resume_point() {
    let entries: Vec<(u64, SessionLogEntry)> = (0..10)
        .map(|i| (i, message(&format!("entry {i}"))))
        .collect();
    let read = resolve(json!({})).expect("valid");
    // The physical JetStream scan stopped at index 5: the log continues.
    let window: Vec<(u64, SessionLogEntry)> = entries[..=5].to_vec();
    let page = read_window_events(
        EventWindow {
            entries: &window,
            capped: true,
            scanned_through: None,
        },
        &read,
        0,
    )
    .expect("page");
    assert_progress(&page, Some(5), Some(6), true);
    assert_eq!(page["events"].as_array().expect("events").len(), 6);
    // The same window without a physical cap implies no more content.
    let page = read_window_events(
        EventWindow {
            entries: &window,
            capped: false,
            scanned_through: None,
        },
        &read,
        0,
    )
    .expect("page");
    assert_eq!(page["truncated"], json!(false));
    assert_eq!(page["next_index"], json!(null));
    // A capped window with an empty filter still progresses the scan.
    let sparse = resolve(json!({ "types": ["cancel"] })).expect("valid");
    let page = read_window_events(
        EventWindow {
            entries: &window,
            capped: true,
            scanned_through: None,
        },
        &sparse,
        0,
    )
    .expect("page");
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["next_index"], json!(6));
    assert_eq!(page["truncated"], json!(true));
}

#[test]
fn physical_scan_progress_survives_purged_only_ranges() {
    // A scan whose whole range was purged still examined every sequence:
    // the page must report that progress, not an implied empty log.
    let read = resolve(json!({})).expect("valid");
    let page = read_window_events(
        EventWindow {
            entries: &[],
            capped: false,
            scanned_through: Some(4),
        },
        &read,
        0,
    )
    .expect("page");
    assert_eq!(page["events"], json!([]));
    assert_progress(&page, Some(4), None, false);
    // When the purge falls inside a capped scan the resume point still
    // follows the physical progress, not the last surviving entry.
    let entries = vec![(1u64, message("survivor"))];
    let page = read_window_events(
        EventWindow {
            entries: &entries,
            capped: true,
            scanned_through: Some(4),
        },
        &read,
        0,
    )
    .expect("page");
    assert_eq!(page["scanned_through"], json!(4));
    assert_eq!(page["next_index"], json!(5));
    // Trailing purged sequences also advance a sparse filtered page, so
    // index_min paging reaches the real tail.
    let sparse = resolve(json!({ "types": ["cancel"] })).expect("valid");
    let page = read_window_events(
        EventWindow {
            entries: &entries,
            capped: false,
            scanned_through: Some(4),
        },
        &sparse,
        0,
    )
    .expect("page");
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["scanned_through"], json!(4));
    assert_eq!(page["next_index"], json!(null));
}

#[test]
fn scan_caps_share_one_source_of_truth_with_the_physical_reader() {
    assert_eq!(
        (SCAN_RECORD_LIMIT, SCAN_BYTE_LIMIT),
        (1_000, 8 * 1024 * 1024)
    );
    // The constants the bounded JetStream reader enforces are the same ones
    // this contract uses; both names must reference one definition.
    assert_eq!(
        (
            crate::nats_session_log::SCAN_RECORD_LIMIT,
            crate::nats_session_log::SCAN_BYTE_LIMIT
        ),
        (SCAN_RECORD_LIMIT, SCAN_BYTE_LIMIT),
    );
}
