use super::*;

#[test]
fn preview_holes_are_recoverable_via_continuation() {
    let body = (0..300).map(|i| format!("line-{i}\n")).collect::<String>();
    let entries = vec![(4u64, message(&body))];
    // Small budget: the single-line-window preview still gets byte-capped.
    let read = resolve(json!({ "max_output_bytes": 1024 })).expect("valid");
    let page = read_events(&entries, &read).expect("page");
    assert!(serialized(&page) <= 1024, "page must respect its budget");
    let ranges = page["omitted_ranges"].as_array().expect("omitted ranges");
    assert!(!ranges.is_empty(), "giant event must report omissions");
    assert_eq!(page["events"][0]["complete"], json!(false));

    let rendered = render_event_text(&entries[0].1);
    // Every reported interval must be recoverable exactly, byte for byte.
    let expected: String = ranges
        .iter()
        .map(|range| {
            let start = range["start"].as_u64().expect("start") as usize;
            let end = range["end"].as_u64().expect("end") as usize;
            &rendered[start..end]
        })
        .collect();
    let mut recovered = String::new();
    for range in ranges {
        let mut offset = range["start"].as_u64().expect("start");
        let end = range["end"].as_u64().expect("end");
        while offset < end {
            let read = resolve(json!({
                "max_output_bytes": 1024,
                "entry_index": 4,
                "byte_offset": offset,
            }))
            .expect("continuation read");
            let chunk = read_continuation(&entries, &read).expect("chunk");
            assert!(serialized(&chunk) <= 1024);
            recovered.push_str(chunk["text"].as_str().expect("text"));
            match chunk["continuation"].as_object() {
                Some(next) => {
                    offset = next["byte_offset"].as_u64().expect("next offset");
                }
                None => break,
            }
        }
    }
    assert_eq!(recovered, expected);
}

#[test]
fn continuation_walk_covers_the_whole_entry_exactly() {
    let body = "héllo 😀 →".repeat(4_000);
    let entries = vec![(3u64, message(&body))];
    let rendered = render_event_text(&entries[0].1);
    let mut collected = String::new();
    let mut offset = 0u64;
    let mut chunks = 0;
    loop {
        let read = resolve(json!({
            "max_output_bytes": 1024,
            "entry_index": 3,
            "byte_offset": offset,
        }))
        .expect("continuation read");
        let chunk = read_continuation(&entries, &read).expect("chunk");
        assert!(serialized(&chunk) <= 1024);
        assert_eq!(chunk["entry_index"], json!(3));
        assert_eq!(chunk["total_bytes"], json!(rendered.len()));
        collected.push_str(chunk["text"].as_str().expect("text"));
        match chunk["continuation"].as_object() {
            Some(next) => {
                offset = next["byte_offset"].as_u64().expect("next offset");
                assert_eq!(chunk["truncated"], json!(true));
            }
            None => {
                assert_eq!(chunk["truncated"], json!(false));
                break;
            }
        }
        chunks += 1;
        assert!(chunks < 5_000, "continuation walk did not converge");
    }
    assert_eq!(collected, rendered, "walk must have no gaps or duplicates");
}

#[test]
fn continuation_validates_entry_and_offset() {
    let entries = vec![(5u64, message("héllo"))];
    let rendered = render_event_text(&entries[0].1);

    let read = resolve(json!({ "entry_index": 9, "byte_offset": 0 })).expect("valid shape");
    let error = read_continuation(&entries, &read).unwrap_err();
    assert_eq!(
        error,
        "read.entry_index 9 is not present in the session log"
    );

    let read = resolve(json!({
        "entry_index": 5,
        "byte_offset": (rendered.len() + 1) as u64,
    }))
    .expect("valid shape");
    let error = read_continuation(&entries, &read).unwrap_err();
    assert!(
        error.starts_with("read.byte_offset") && error.contains("is out of range for entry 5"),
        "unexpected out-of-range error: {error}"
    );

    let mid_character = rendered.find('é').expect("multibyte character") + 1;
    let read = resolve(json!({
        "entry_index": 5,
        "byte_offset": mid_character as u64,
    }))
    .expect("valid shape");
    let error = read_continuation(&entries, &read).unwrap_err();
    assert!(
        error.contains("is not on a UTF-8 character boundary in entry 5"),
        "unexpected boundary error: {error}"
    );
}

#[test]
fn continuation_at_entry_end_delivers_an_empty_final_chunk() {
    let entries = vec![(5u64, message("hello"))];
    let rendered = render_event_text(&entries[0].1);
    let read = resolve(json!({
        "entry_index": 5,
        "byte_offset": rendered.len() as u64,
    }))
    .expect("valid shape");
    let chunk = read_continuation(&entries, &read).expect("chunk");
    assert_eq!(chunk["text"], json!(""));
    assert_eq!(chunk["truncated"], json!(false));
    assert_eq!(chunk["continuation"], json!(null));
}
