use super::*;

#[test]
fn escaped_char_len_matches_serde_json_exactly() {
    let torture =
        "plain \"quoted\" \\backslash \n\t\r\u{8}\u{c} \u{1}\u{1f}\u{7f} héllo → 😀 emoji";
    let mut measured = 0;
    for c in torture.chars() {
        measured += escaped_char_len(c);
    }
    let actual = serde_json::to_string(&Value::String(torture.to_string()))
        .expect("string serializes")
        .len();
    assert_eq!(
        measured + 2,
        actual,
        "escape accounting drifted from serde_json"
    );
}

#[test]
fn head_tail_preview_keeps_line_ends_and_reports_the_hole() {
    // "line-0\n" (7B), "line-10\n" (8B), "line-100\n" (9B)...
    let text = (0..300).map(|i| format!("line-{i}\n")).collect::<String>();
    assert_eq!(text.len(), 2_590);
    let (delivered, omitted) = deliver_event(&text, 2, 3, usize::MAX);
    // Head keeps lines 0-1; tail keeps lines 297-299.
    assert_eq!(delivered, vec![(0, 14), (2_563, 2_590)]);
    assert_eq!(omitted, vec![(14, 2_563)]);
    let rendered: String = delivered
        .iter()
        .map(|(start, end)| &text[*start..*end])
        .collect();
    assert!(rendered.starts_with("line-0\nline-1\n"));
    assert!(rendered.ends_with("line-297\nline-298\nline-299\n"));

    // Within the line window nothing is preview-trimmed.
    let (delivered, omitted) = deliver_event("short text\nsecond line\n", 25, 75, usize::MAX);
    assert_eq!(delivered, vec![(0, 23)]);
    assert!(omitted.is_empty());
}

#[test]
fn budget_fit_never_drops_the_first_matched_event() {
    let entries = vec![
        (7u64, message(&"x".repeat(200_000))),
        (8u64, message("small")),
    ];
    let read = resolve(json!({ "max_output_bytes": 1024 })).expect("valid");
    let page = read_events(&entries, &read).expect("page");
    assert!(serialized(&page) <= 1024, "page must respect its budget");
    let events = page["events"].as_array().expect("events");
    assert!(!events.is_empty(), "first matched event must be delivered");
    assert_eq!(events[0]["index"], json!(7));
    assert_eq!(events[0]["complete"], json!(false));
    // Delivered text is a prefix of the canonical rendering, cut on a
    // char boundary (String slices cannot be invalid UTF-8).
    let rendered = render_event_text(&entries[0].1);
    assert!(rendered.starts_with(events[0]["text"].as_str().expect("text")));
}

#[test]
fn budget_fit_pops_tail_events_and_reports_resume_index() {
    let entries: Vec<(u64, SessionLogEntry)> = (0..50)
        .map(|i| (i, message(&format!("event {i}: {}", "y".repeat(800)))))
        .collect();
    let read = resolve(json!({ "max_output_bytes": 2048 })).expect("valid");
    let page = read_events(&entries, &read).expect("page");
    assert!(serialized(&page) <= 2048, "page must respect its budget");
    let events = page["events"].as_array().expect("events");
    assert!(!events.is_empty());
    // Delivered rows form a contiguous prefix of the matched events.
    let first = events[0]["index"].as_u64().expect("index");
    let last = events.last().expect("non-empty")["index"]
        .as_u64()
        .expect("index");
    assert_eq!(events.len(), (last - first + 1) as usize);
    assert_eq!(page["next_index"], json!(last + 1));
    assert_eq!(page["truncated"], json!(true));
}

#[test]
fn non_ascii_output_stays_valid_and_bounded() {
    let body = "héllo 😀 →".repeat(4_000);
    let entries = vec![(2u64, message(&body))];
    let read = resolve(json!({ "max_output_bytes": 1024 })).expect("valid");
    let page = read_events(&entries, &read).expect("page");
    assert!(serialized(&page) <= 1024);
    let events = page["events"].as_array().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["complete"], json!(false));
    // Round-trip through JSON proves the text is valid UTF-8 at a boundary.
    let text = events[0]["text"].as_str().expect("text");
    let rendered = render_event_text(&entries[0].1);
    assert!(rendered.starts_with(text));
}

#[test]
fn outer_reserve_shrinks_the_fitting_budget() {
    let entries: Vec<(u64, SessionLogEntry)> = (0..40)
        .map(|i| (i, message(&format!("event {i}: {}", "y".repeat(300)))))
        .collect();
    let read = resolve(json!({ "max_output_bytes": 2048 })).expect("valid");
    let reserve = 600;
    let page = read_window_events(
        EventWindow {
            entries: &entries,
            capped: false,
            scanned_through: None,
        },
        &read,
        reserve,
    )
    .expect("page");
    let len = serialized(&page);
    assert!(
        len + reserve <= 2048,
        "enclosing result must respect the budget: {len}+{reserve}"
    );
    // Without the reserve the page legitimately uses more of the budget.
    let unreserved = read_window_events(
        EventWindow {
            entries: &entries,
            capped: false,
            scanned_through: None,
        },
        &read,
        0,
    )
    .expect("page");
    assert!(
        serialized(&unreserved) > len,
        "reserve should shrink the delivered page"
    );
}

#[test]
fn impossible_outer_reserve_is_rejected() {
    let entries = vec![(1u64, message("hello"))];
    let read = resolve(json!({ "max_output_bytes": 1024 })).expect("valid");
    let error = read_window_events(
        EventWindow {
            entries: &entries,
            capped: false,
            scanned_through: None,
        },
        &read,
        1024,
    )
    .unwrap_err();
    assert_eq!(
        error,
        "enclosing metadata reserve (1024 bytes) leaves no max_output_bytes \
         budget for the read result (1024 bytes)"
    );
    // A reserve that leaves room for the page is accepted.
    read_window_events(
        EventWindow {
            entries: &entries,
            capped: false,
            scanned_through: None,
        },
        &read,
        512,
    )
    .expect("fits");
}

#[test]
fn continuation_reserves_enclosing_metadata() {
    let entries = vec![(3u64, message(&"x".repeat(5_000)))];
    let read = resolve(json!({
        "max_output_bytes": 1024,
        "entry_index": 3,
        "byte_offset": 0,
    }))
    .expect("valid");
    let reserve = 200;
    let chunk = read_window_continuation(&entries, &read, reserve).expect("chunk");
    assert!(
        serialized(&chunk) + reserve <= 1024,
        "enclosing continuation result must respect the budget"
    );
    let error = read_window_continuation(&entries, &read, 1024).unwrap_err();
    assert!(
        error.contains("leaves no max_output_bytes budget"),
        "unexpected reserve error: {error}"
    );
}
