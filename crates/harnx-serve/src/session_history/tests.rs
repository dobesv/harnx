use super::{params::*, projection::Projection, *};
use harnx_core::session::{Session, SessionLogEntry};

fn fixture() -> Vec<(u64, SessionLogEntry)> {
    crate::transcript_snapshot::tests::fixture(23)
}

fn window(selector: Selector, entries: &[(u64, SessionLogEntry)]) -> Window {
    Window {
        version: 1,
        scope: ("local".into(), "plain".into(), "history".into()),
        created_at: "test".into(),
        as_of_seq: entries.last().map_or(0, |(seq, _)| *seq),
        selector,
    }
}

fn first(entries: &[(u64, SessionLogEntry)], selector: Selector, limit: usize) -> Value {
    let window = window(selector, entries);
    let projection = Projection::build(entries, &Session::default(), &window).unwrap();
    page(
        &projection,
        Cursor {
            window,
            offset: 0,
            limit,
        },
    )
}

fn page(projection: &Projection, cursor: Cursor) -> Value {
    serde_json::to_value(projection.page(&cursor).unwrap()).unwrap()
}

fn cursor(value: &Value) -> Cursor {
    let params: HistoryParams = serde_json::from_value(json!({"cursor": value})).unwrap();
    let Request::Continue(cursor) = params.parse().unwrap() else {
        panic!("cursor expected")
    };
    cursor
}

fn rows(value: &Value) -> &[Value] {
    value["history_snapshot"].as_array().unwrap()
}

fn walk(entries: &[(u64, SessionLogEntry)], selector: Selector, limit: usize) -> Vec<Value> {
    let window = window(selector, entries);
    let projection = Projection::build(entries, &Session::default(), &window).unwrap();
    let mut current = page(
        &projection,
        Cursor {
            window,
            offset: 0,
            limit,
        },
    );
    let mut result = Vec::new();
    loop {
        assert_rounds_complete(rows(&current));
        result.extend_from_slice(rows(&current));
        if current["next_cursor"].is_null() {
            break;
        }
        current = page(&projection, cursor(&current["next_cursor"]));
    }
    result
}

fn assert_rounds_complete(rows: &[Value]) {
    let calls: Vec<_> = rows
        .iter()
        .filter_map(|row| row["toolCalls"].as_array())
        .flatten()
        .map(|call| &call["id"])
        .collect();
    let results: Vec<_> = rows
        .iter()
        .filter(|row| row["role"] == "tool")
        .map(|row| &row["toolCallId"])
        .collect();
    assert_eq!(calls, results);
}

#[test]
fn all_limits_walk_both_archives_once_without_splitting_tool_rounds() {
    let entries = fixture();
    for compaction in [15, 5] {
        let selector = Selector::Before(compaction);
        let expected = rows(&first(&entries, selector.clone(), 100)).to_vec();
        for limit in 1..=8 {
            assert_eq!(walk(&entries, selector.clone(), limit), expected);
        }
    }
}

#[test]
fn marker_metadata_matches_session_get_and_only_accompanies_marker_page() {
    let entries = fixture();
    let first = first(&entries, Selector::Before(15), 1);
    let expected = crate::transcript_snapshot::HistorySnapshot::from_entries(
        &entries[..6],
        &Session::default(),
        false,
    )
    .unwrap();
    assert_eq!(first["compaction"], json!(expected.compaction));
    assert_eq!(
        rows(&first),
        expected
            .messages
            .iter()
            .map(|message| json!(message))
            .collect::<Vec<_>>()
    );
    assert_eq!(first["earlier_compaction"], 5);
}

#[test]
fn next_and_previous_cursors_restore_identical_pages() {
    let entries = fixture();
    let first = first(&entries, Selector::Before(15), 2);
    let next = cursor(&first["next_cursor"]);
    let projection = Projection::build(&entries, &Session::default(), &next.window).unwrap();
    let second = page(&projection, next);
    assert!(second.get("compaction").is_none());
    assert_eq!(second["earlier_compaction"], 5);
    assert_eq!(page(&projection, cursor(&second["prev_cursor"])), first);
}

#[test]
fn ranges_reaching_either_half_of_a_tool_round_return_the_whole_round() {
    let entries = fixture();
    let expected = first(&entries, Selector::Range { from: 8, to: 9 }, 1);
    for seq in [8, 9] {
        assert_eq!(
            first(&entries, Selector::Range { from: seq, to: seq }, 1),
            expected
        );
    }
    assert_eq!(rows(&expected).len(), 2);
    assert_rounds_complete(rows(&expected));
}

#[test]
fn range_marker_and_empty_control_only_ranges() {
    let entries = fixture();
    let marker = first(&entries, Selector::Range { from: 5, to: 6 }, 1);
    let empty = first(&entries, Selector::Range { from: 6, to: 6 }, 1);
    assert_eq!(rows(&marker).len(), 1);
    assert!(rows(&empty).is_empty());
    assert!(empty.get("compaction").is_none());
}

#[test]
fn invalid_selector_and_limit_params_have_clear_errors() {
    for params in [
        json!({}),
        json!({"before_compaction":0}),
        json!({"from_seq":1}),
        json!({"from_seq":2,"to_seq":1}),
        json!({"before_compaction":5,"from_seq":1,"to_seq":2}),
        json!({"before_compaction":5,"limit":0}),
        json!({"before_compaction":5,"limit":1001}),
        json!({"cursor":"not base64"}),
    ] {
        let params: HistoryParams = serde_json::from_value(params).unwrap();
        assert!(params.parse().is_err());
    }
}

#[test]
fn invalid_sequence_windows_are_rejected() {
    let entries = fixture();
    for selector in [
        Selector::Before(1),
        Selector::Before(999),
        Selector::Range { from: 1, to: 24 },
        Selector::Range { from: 1, to: 16 },
    ] {
        let result = Projection::build(&entries, &Session::default(), &window(selector, &entries));
        assert!(result.is_err());
    }
}

#[test]
fn malformed_cursor_offsets_limits_and_versions_are_rejected() {
    let entries = fixture();
    let current = cursor(&first(&entries, Selector::Before(15), 2)["next_cursor"]);
    let mut invalid = current.clone();
    invalid.offset = 1;
    assert_invalid_cursor(invalid);
    let mut invalid = current.clone();
    invalid.limit = 0;
    assert_invalid_cursor(invalid);
    let mut invalid = current;
    invalid.window.version = 2;
    assert_invalid_cursor(invalid);
}

fn assert_invalid_cursor(cursor: Cursor) {
    let params: HistoryParams = serde_json::from_value(json!({"cursor":cursor.encode()})).unwrap();
    assert!(params.parse().is_err());
}

#[test]
fn cursor_selectors_and_changed_limits_are_rejected() {
    let value = first(&fixture(), Selector::Before(15), 2)["next_cursor"].clone();
    for params in [
        json!({"cursor":value,"before_compaction":15}),
        json!({"cursor":value,"limit":3}),
    ] {
        assert!(serde_json::from_value::<HistoryParams>(params)
            .unwrap()
            .parse()
            .is_err());
    }
}

#[test]
fn cursor_rebuild_is_frozen_before_later_edits_and_rewinds() {
    let mut entries = fixture();
    let current = cursor(&first(&entries, Selector::Range { from: 15, to: 22 }, 1)["next_cursor"]);
    let original = Projection::build(&entries, &Session::default(), &current.window).unwrap();
    entries.push((24, SessionLogEntry::Rewind { after_seq: 4 }));
    let rebuilt = Projection::build(&entries, &Session::default(), &current.window).unwrap();
    assert_eq!(page(&original, current.clone()), page(&rebuilt, current));
}

#[test]
fn replacement_rows_sharing_seq_keep_order_across_pages() {
    let entries = edited_fixture();
    let all = walk(&entries, Selector::Range { from: 15, to: 24 }, 1);
    let texts: Vec<_> = all
        .iter()
        .filter_map(|row| row["content"].as_str())
        .collect();
    assert_eq!(
        &texts[texts.len() - 3..],
        [
            "replacement one",
            "replacement two",
            "The earlier window contains the original prompt and both tool rounds."
        ]
    );
    assert_eq!(
        walk(&entries, Selector::Range { from: 24, to: 24 }, 1).len(),
        2
    );
}

fn edited_fixture() -> Vec<(u64, SessionLogEntry)> {
    let mut entries = fixture();
    let replacements = ["replacement one", "replacement two"]
        .map(|text| json!({"type":"message","role":"user","content":text}).to_string())
        .to_vec();
    entries.push((
        24,
        SessionLogEntry::EditEntries {
            from: 21,
            to: 21,
            replacements,
        },
    ));
    entries
}

#[test]
fn first_entry_compaction_has_empty_page_without_cursors() {
    let entries = vec![(
        1,
        SessionLogEntry::Compress {
            prompt: "empty archive".into(),
        },
    )];
    let page = first(&entries, Selector::Before(1), 1);
    assert!(rows(&page).is_empty());
    assert_eq!(page["next_cursor"], Value::Null);
    assert_eq!(page["prev_cursor"], Value::Null);
}

#[test]
fn http_query_validation_matches_rpc_params() {
    assert!(query_params(Some("before_compaction=15&limit=2"))
        .unwrap()
        .parse()
        .is_ok());
    for query in [
        "limit=1&limit=2",
        "limit=-1",
        "unknown=5",
        "limit=abc",
        "limit=1.5",
    ] {
        assert!(query_params(Some(query)).is_err(), "{query}");
    }
}

#[test]
fn multiple_calls_and_results_stay_one_group() {
    let mut entries = fixture();
    let SessionLogEntry::ToolCalls { calls, .. } = &mut entries[7].1 else {
        panic!()
    };
    let mut call = calls[0].clone();
    call.id = Some("second-call".into());
    calls.push(call);
    let SessionLogEntry::ToolResults { results, .. } = &mut entries[8].1 else {
        panic!()
    };
    let mut result = results[0].clone();
    result.id = Some("second-call".into());
    results.push(result);
    let page = first(&entries, Selector::Range { from: 9, to: 9 }, 1);
    assert_eq!(rows(&page).len(), 3);
    assert_rounds_complete(rows(&page));
}

#[test]
fn tool_replacements_share_sequence_without_losing_later_messages() {
    let mut entries = fixture();
    let replacements: Vec<_> = [
        entries[16].1.clone(),
        entries[17].1.clone(),
        entries[20].1.clone(),
    ]
    .iter()
    .map(|entry| json!(entry).to_string())
    .collect();
    entries.push((
        24,
        SessionLogEntry::EditEntries {
            from: 21,
            to: 21,
            replacements,
        },
    ));
    let page = walk(&entries, Selector::Range { from: 24, to: 24 }, 1);
    assert_eq!(page.len(), 3);
    assert_rounds_complete(&page);
    assert_eq!(
        page[2]["content"],
        "Show the previous compaction's history."
    );
}

#[test]
fn cache_reuses_projection_and_evicts_least_recent_window() {
    use std::sync::Arc;
    let entries = fixture();
    let key = window(Selector::Before(15), &entries);
    let projection = Arc::new(Projection::build(&entries, &Session::default(), &key).unwrap());
    let mut cache = HistoryCache::default();
    cache.insert(key.clone(), Arc::clone(&projection));
    assert!(Arc::ptr_eq(&projection, &cache.get(&key).unwrap()));
    for seq in 1..=17 {
        let mut alternate = key.clone();
        alternate.selector = Selector::Range { from: seq, to: seq };
        cache.insert(alternate, Arc::clone(&projection));
    }
    assert!(cache.get(&key).is_none());
}

#[test]
fn cursors_past_the_end_are_rejected() {
    let entries = fixture();
    let mut cursor = cursor(&first(&entries, Selector::Before(15), 1)["next_cursor"]);
    cursor.offset = usize::MAX;
    let projection = Projection::build(&entries, &Session::default(), &cursor.window).unwrap();
    assert!(projection.page(&cursor).is_err());
}

#[test]
fn orphaned_tool_round_rebuild_has_stable_ids_and_no_split() {
    let mut entries = fixture();
    entries.truncate(8);
    let selector = Selector::Range { from: 5, to: 8 };
    let first = walk(&entries, selector.clone(), 1);
    let rebuilt = walk(&entries, selector, 1);
    assert_eq!(first, rebuilt);
    assert_eq!(first.len(), 4);
}

#[test]
fn malformed_cursor_payloads_return_errors_without_panics() {
    use base64::Engine;
    for bytes in [vec![255, 254], b"{}".to_vec(), vec![]] {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let params: HistoryParams = serde_json::from_value(json!({"cursor":encoded})).unwrap();
        assert!(params.parse().is_err());
    }
}

#[test]
fn terminal_archive_json_matches_documented_wire_contract() {
    let result = first(&fixture(), Selector::Before(5), 100);
    assert_eq!(
        result,
        json!({
            "history_snapshot": [
                {"role":"user","id":"f184b0f4-5008-5705-8979-31bf8cc66c05","content":"Inspect the session log format."},
                {"role":"assistant","id":"ac5ec19d-b65e-5eca-92af-74349ef65e1f","content":"Tool calls and results are separate log entries."}
            ],
            "next_cursor":null, "prev_cursor":null, "earlier_compaction":null
        })
    );
}

#[test]
fn ranges_ignore_rewound_compactions_and_removed_messages() {
    let entries = [
        message(1),
        message(2),
        compress(3),
        message(4),
        (5, SessionLogEntry::Rewind { after_seq: 1 }),
        message(6),
    ];
    let subset = first(&entries, Selector::Range { from: 1, to: 2 }, 100);
    let current = first(&entries, Selector::Range { from: 1, to: 6 }, 100);
    assert_eq!(content_texts(&subset), ["message 1"]);
    assert_eq!(content_texts(&current), ["message 1", "message 6"]);
    assert_eq!(
        content_texts(&first(&entries, Selector::Before(3), 100)),
        ["message 1", "message 2"]
    );
}

#[test]
fn ranges_do_not_cross_removed_compaction_boundaries() {
    let entries = [
        message(1),
        compress(2),
        message(3),
        compress(4),
        message(5),
        (6, SessionLogEntry::Rewind { after_seq: 1 }),
        message(7),
    ];
    let subset = first(&entries, Selector::Range { from: 1, to: 3 }, 100);
    let current = first(&entries, Selector::Range { from: 1, to: 7 }, 100);
    assert_eq!(content_texts(&subset), ["message 1"]);
    assert_eq!(content_texts(&current), ["message 1", "message 7"]);
    assert!(subset.get("compaction").is_none());
}

fn message(seq: u64) -> (u64, SessionLogEntry) {
    (
        seq,
        serde_json::from_value(json!({
            "type":"message", "id":format!("message-{seq}"), "role":"user",
            "content":format!("message {seq}")
        }))
        .unwrap(),
    )
}

fn compress(seq: u64) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::Compress {
            prompt: format!("summary {seq}"),
        },
    )
}

fn content_texts(value: &Value) -> Vec<&str> {
    rows(value)
        .iter()
        .map(|row| row["content"].as_str().unwrap())
        .collect()
}

#[test]
fn terminal_page_cursor_returns_to_the_penultimate_page() {
    let entries = fixture();
    let first = first(&entries, Selector::Before(15), 2);
    let next = cursor(&first["next_cursor"]);
    let projection = Projection::build(&entries, &Session::default(), &next.window).unwrap();
    let second = page(&projection, next);
    let terminal = page(&projection, cursor(&second["next_cursor"]));
    assert_eq!(first["prev_cursor"], Value::Null);
    assert_eq!(terminal["next_cursor"], Value::Null);
    assert_eq!(page(&projection, cursor(&terminal["prev_cursor"])), second);
}

#[test]
fn cursor_length_guard_reports_oversized_payloads() {
    let params = HistoryParams {
        cursor: Some("a".repeat(4097)),
        ..Default::default()
    };
    let error = params.parse().err().expect("oversized cursor rejected");
    assert_eq!(error.to_string(), "history cursor is too long");
}

#[test]
fn forged_cursor_boundaries_must_exist_in_the_raw_log() {
    let mut entries = fixture();
    entries.retain(|(seq, _)| *seq != 13);
    let valid = cursor(&first(&entries, Selector::Before(15), 1)["next_cursor"]);
    for as_of_seq in [13, 24, u64::MAX] {
        let mut forged = valid.clone();
        forged.window.as_of_seq = as_of_seq;
        let decoded = cursor(&json!(forged.encode()));
        let error = Projection::build(&entries, &Session::default(), &decoded.window)
            .err()
            .expect("nonexistent cursor boundary rejected");
        assert_eq!(
            error.to_string(),
            "history cursor snapshot is no longer available"
        );
    }
}

#[test]
fn unknown_http_query_parameters_are_rejected_before_numeric_parsing() {
    for query in ["debug=true", "debug=1", "debug="] {
        let error = query_params(Some(query)).expect_err("unknown query rejected");
        assert_eq!(error.to_string(), "unknown history parameter: debug");
    }
}

#[test]
fn ranges_ignore_deleted_compaction_markers() {
    let entries = [
        message(1),
        message(2),
        compress(3),
        message(4),
        (
            5,
            SessionLogEntry::EditEntries {
                from: 2,
                to: 3,
                replacements: vec![],
            },
        ),
    ];
    let result = first(&entries, Selector::Range { from: 1, to: 2 }, 100);
    assert_eq!(content_texts(&result), ["message 1"]);
    assert!(result.get("compaction").is_none());
}
