use super::*;

#[derive(Default)]
struct ReadSpec {
    style: ResultStyle,
    line_numbers: Option<bool>,
    max_output_bytes: Option<usize>,
    offset: Option<usize>,
    limit: Option<usize>,
    tail: Option<usize>,
    grep: Option<String>,
}

async fn read_fixture(
    file_name: &str,
    content: &[u8],
    spec: ReadSpec,
) -> Result<CallToolResult, ErrorData> {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, file_name, content);
    make_server(temp_dir.path())
        .read_file_impl(
            ReadFileParams {
                path: path_string(&file_path),
                offset: spec.offset,
                limit: spec.limit,
                tail: spec.tail,
                grep: spec.grep,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: spec.max_output_bytes,
                line_numbers: spec.line_numbers,
            },
            spec.style,
        )
        .await
}

#[tokio::test]
async fn test_read_file_with_offset_limit() {
    let result = read_fixture(
        "offset.txt",
        b"one\ntwo\nthree\nfour\n",
        ReadSpec {
            offset: Some(2),
            limit: Some(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let text = text_content(&result);
    assert!(text.contains("2: two"));
    assert!(text.contains("3: three"));
    assert!(text.contains("Use offset=4 to continue"));
}

#[tokio::test]
async fn test_read_file_with_grep() {
    let result = read_fixture(
        "grep.txt",
        b"alpha\nmatch-one\nbeta\nmatch-two\n",
        ReadSpec {
            grep: Some("match".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let text = text_content(&result);
    assert!(text.contains("2: match-one"));
    assert!(text.contains("4: match-two"));
    assert!(!text.contains("1: alpha"));
}

#[tokio::test]
async fn test_read_file_with_tail() {
    let result = read_fixture(
        "tail.txt",
        b"one\ntwo\nthree\nfour\n",
        ReadSpec {
            tail: Some(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let text = text_content(&result);
    assert!(text.contains("3: three"));
    assert!(text.contains("4: four"));
    assert!(text.contains("showing last 2 of 4 matching lines"));
}

/// Read `content` with combined `offset`+`tail` and return the rendered text.
async fn read_offset_tail(content: &str, offset: usize, tail: usize) -> String {
    let result = read_fixture(
        "offset_tail.txt",
        content.as_bytes(),
        ReadSpec {
            offset: Some(offset),
            tail: Some(tail),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    text_content(&result)
}

#[tokio::test]
async fn test_read_file_offset_and_tail_combinations() {
    let six = "one\ntwo\nthree\nfour\nfive\nsix\n";

    // Skip to line 3, then tail the last 2 lines of the remaining window
    // (lines 3..6) → lines 5 and 6. Tail is anchored to the end, so no
    // forward "more matching lines" notice.
    let text = read_offset_tail(six, 3, 2).await;
    for expect in ["5: five", "6: six", "showing last 2 of 4 matching lines"] {
        assert!(text.contains(expect), "expected {expect:?} in: {text}");
    }
    for absent in ["4: four", "more matching lines"] {
        assert!(!text.contains(absent), "unexpected {absent:?} in: {text}");
    }

    // tail == window_len (offset=3 leaves a 4-line window): whole window, no
    // "showing last" notice.
    let text = read_offset_tail(six, 3, 4).await;
    assert_window_without_notice(&text);

    // tail > window_len (offset=3 leaves a 2-line window on a 4-line file):
    // whole window, no notice.
    let text = read_offset_tail("one\ntwo\nthree\nfour\n", 3, 5).await;
    assert_window_without_notice(&text);
}

/// Asserts a post-offset window starting at line 3 was returned in full with
/// no truncation notice.
fn assert_window_without_notice(text: &str) {
    assert!(text.contains("3: three"), "got: {text}");
    assert!(!text.contains("2: two"), "got: {text}");
    assert!(!text.contains("showing last"), "got: {text}");
}

#[tokio::test]
async fn test_read_file_offset_beyond_eof_with_tail_errors() {
    let result = read_fixture(
        "offset_tail_eof.txt",
        b"one\ntwo\nthree\n",
        ReadSpec {
            offset: Some(4),
            tail: Some(2),
            ..Default::default()
        },
    )
    .await;

    let message = result.expect_err("expected error").message;
    assert!(
        message.contains("beyond end of result set"),
        "got: {message}"
    );
}

#[tokio::test]
async fn test_read_file_offset_zero_rejected() {
    let result = read_fixture(
        "offset_zero.txt",
        b"one\ntwo\n",
        ReadSpec {
            offset: Some(0),
            ..Default::default()
        },
    )
    .await;

    let message = result.expect_err("expected error").message;
    assert!(message.contains("offset must be >= 1"), "got: {message}");
}

#[tokio::test]
async fn test_read_file_binary_detection() {
    let result = read_fixture("binary.bin", b"hello\0world", ReadSpec::default())
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("appears to be a binary file"));
}

#[tokio::test]
async fn test_read_file_image_png() {
    let result = read_fixture(
        "test.png",
        b"\x89PNG\r\n\x1a\n...fake...",
        ReadSpec::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.is_error, Some(false));
    let mut found_image = false;
    for content in result.content {
        let v = serde_json::to_value(&content).unwrap();
        if v["type"] == "image" {
            found_image = true;
            assert_eq!(v["mimeType"], "image/png");
            assert!(!v["data"].as_str().unwrap().is_empty());
        }
    }
    assert!(found_image, "expected to find an Image content block");
}

#[tokio::test]
async fn test_read_file_image_oversized() {
    let mut file_data = vec![0xFF, 0xD8, 0xFF, 0x00];
    file_data.resize(5 * 1024 * 1024 + 10, 0);
    let result = read_fixture("big.jpg", &file_data, ReadSpec::default())
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("image too large"));
}

#[test]
fn test_detect_image_mime_logic() {
    let p = Path::new("test.txt");
    assert_eq!(
        FsServer::detect_image_mime(p, b"\x89PNG\r\n\x1a\n123"),
        Some("image/png")
    );
    assert_eq!(
        FsServer::detect_image_mime(p, b"\xFF\xD8\xFF123"),
        Some("image/jpeg")
    );
    assert_eq!(
        FsServer::detect_image_mime(p, b"GIF87a123"),
        Some("image/gif")
    );
    assert_eq!(
        FsServer::detect_image_mime(p, b"GIF89a123"),
        Some("image/gif")
    );
    assert_eq!(
        FsServer::detect_image_mime(p, b"RIFF1234WEBP123"),
        Some("image/webp")
    );

    // Extension fallback
    assert_eq!(
        FsServer::detect_image_mime(Path::new("test.png"), b"random"),
        Some("image/png")
    );
    assert_eq!(
        FsServer::detect_image_mime(Path::new("TEST.JPG"), b"random"),
        Some("image/jpeg")
    );
    assert_eq!(
        FsServer::detect_image_mime(Path::new("file.webp"), b"random"),
        Some("image/webp")
    );

    // Neither
    assert_eq!(
        FsServer::detect_image_mime(Path::new("test.txt"), b"random"),
        None
    );
}

fn script_spec() -> ReadSpec {
    ReadSpec {
        style: ResultStyle::Script,
        ..Default::default()
    }
}

fn fields(result: &CallToolResult) -> &Value {
    result.structured_content.as_ref().unwrap()
}

#[tokio::test]
async fn model_read_is_unchanged_without_line_numbers_param() {
    let result = read_fixture("three.txt", b"a\nb\nc\n", ReadSpec::default())
        .await
        .unwrap();
    assert_eq!(text_content(&result), "1: a\n2: b\n3: c");
    assert!(result.structured_content.is_none());
}

#[tokio::test]
async fn line_numbers_false_removes_prefixes_in_a_direct_call() {
    let spec = ReadSpec {
        line_numbers: Some(false),
        ..Default::default()
    };
    let result = read_fixture("three.txt", b"a\nb\nc\n", spec).await.unwrap();
    assert_eq!(text_content(&result), "a\nb\nc");
}

#[tokio::test]
async fn script_read_returns_plain_text_and_fields() {
    let result = read_fixture("three.txt", b"a\nb\nc\n", script_spec())
        .await
        .unwrap();
    let f = fields(&result);
    assert_eq!(f["text"], "a\nb\nc\n");
    assert_eq!(f["notes"], json!([]));
    assert_eq!(f["total_lines"], 3);
    assert_eq!(f["start_line"], 1);
    assert_eq!(f["end_line"], 3);
    assert_eq!(result.content.len(), 1);
    assert_eq!(audience(&result.content[0]), Some(&vec![Role::User]));
}

#[tokio::test]
async fn script_read_of_an_empty_file_has_empty_text() {
    let result = read_fixture("empty.txt", b"", script_spec()).await.unwrap();
    assert_eq!(fields(&result)["text"], "");
    assert_eq!(fields(&result)["total_lines"], 0);
}

#[tokio::test]
async fn script_read_reports_the_line_cap_in_notes() {
    let spec = ReadSpec {
        limit: Some(2),
        ..script_spec()
    };
    let result = read_fixture("long.txt", b"a\nb\nc\nd\n", spec)
        .await
        .unwrap();
    let f = fields(&result);
    assert_eq!(f["text"], "a\nb\n");
    let notes = f["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].as_str().unwrap().contains("offset=3"));
}

#[tokio::test]
async fn script_read_stops_at_a_whole_line_within_the_byte_cap() {
    let content = (1..=200)
        .map(|n| format!("line number {n}\n"))
        .collect::<String>();
    let spec = ReadSpec {
        max_output_bytes: Some(1024),
        ..script_spec()
    };
    let result = read_fixture("big.txt", content.as_bytes(), spec)
        .await
        .unwrap();
    let f = fields(&result);
    let text = f["text"].as_str().unwrap();
    assert!(text.len() <= 1024, "got {} bytes", text.len());
    let last = text.lines().last().unwrap();
    assert!(content.lines().any(|l| l == last));
    let end = f["end_line"].as_u64().unwrap();
    assert_eq!(text.lines().count() as u64, end);
    let note = f["notes"][0].as_str().unwrap();
    assert!(note.contains(&format!("offset={}", end + 1)), "got {note}");
}

#[tokio::test]
async fn script_read_with_line_numbers_true_keeps_prefixes() {
    let spec = ReadSpec {
        line_numbers: Some(true),
        ..script_spec()
    };
    let result = read_fixture("three.txt", b"a\nb\nc\n", spec).await.unwrap();
    assert_eq!(fields(&result)["text"], "1: a\n2: b\n3: c\n");
}

#[tokio::test]
async fn script_read_with_head_and_tail_drops_the_middle() {
    let content = b"1\n2\n3\n4\n5\n6\n";
    let result = read_fixture("ht.txt", content, script_spec())
        .await
        .unwrap();
    assert_eq!(fields(&result)["text"], "1\n2\n3\n4\n5\n6\n");
    let result = make_server_read(content, |p| {
        p.head_lines = Some(1);
        p.tail_lines = Some(2);
    })
    .await;
    let f = fields(&result);
    assert_eq!(f["text"], "1\n5\n6\n");
    assert_eq!(f["notes"], json!(["3 lines left out"]));
}

async fn make_server_read(
    content: &[u8],
    edit: impl FnOnce(&mut ReadFileParams),
) -> CallToolResult {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, "x.txt", content);
    let mut params: ReadFileParams =
        serde_json::from_value(json!({"path": path_string(&file_path)})).unwrap();
    edit(&mut params);
    make_server(temp_dir.path())
        .read_file_impl(params, ResultStyle::Script)
        .await
        .unwrap()
}

#[tokio::test]
async fn script_read_keeps_control_characters_and_carriage_returns() {
    let content = b"a\x1b[31mb\r\nc\r\n";
    let result = read_fixture("ctl.txt", content, script_spec())
        .await
        .unwrap();
    assert_eq!(fields(&result)["text"], "a\u{1b}[31mb\r\nc\r\n");
    assert_eq!(fields(&result)["total_lines"], 2);
    let model = read_fixture("ctl.txt", content, ReadSpec::default())
        .await
        .unwrap();
    assert_eq!(text_content(&model), "1: a[31mb\n2: c");
}

#[tokio::test]
async fn script_read_keeps_a_trailing_blank_line() {
    let result = read_fixture("blank.txt", b"a\n\n", script_spec())
        .await
        .unwrap();
    assert_eq!(fields(&result)["text"], "a\n\n");
    assert_eq!(fields(&result)["total_lines"], 2);
}

#[tokio::test]
async fn script_read_of_a_file_without_a_final_newline() {
    let result = read_fixture("nonl.txt", b"a\nb", script_spec())
        .await
        .unwrap();
    assert_eq!(fields(&result)["text"], "a\nb");
}

#[tokio::test]
async fn script_read_cuts_a_single_line_longer_than_the_byte_cap() {
    let line = "h\u{e9}llo\u{4e16}".repeat(1800);
    assert!(line.chars().count() >= 10_000);
    let spec = ReadSpec {
        max_output_bytes: Some(1024),
        ..script_spec()
    };
    let result = read_fixture("long.txt", format!("{line}\nnext\n").as_bytes(), spec)
        .await
        .unwrap();
    let f = fields(&result);
    let text = f["text"].as_str().unwrap();
    assert!(text.len() <= 1024 && !text.is_empty());
    assert!(line.starts_with(text));
    assert_eq!(f["end_line"], 1);
    assert!(f["notes"][0].as_str().unwrap().contains("offset=2"));
}

#[tokio::test]
async fn script_read_combines_head_tail_and_the_byte_cap() {
    let content = (1..=100)
        .map(|n| format!("line number {n}\n"))
        .collect::<String>();
    let result = make_server_read(content.as_bytes(), |p| {
        p.head_lines = Some(40);
        p.tail_lines = Some(40);
        p.max_output_bytes = Some(256);
    })
    .await;
    let f = fields(&result);
    assert!(f["text"].as_str().unwrap().len() <= 256);
    let notes = f["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0], "20 lines left out");
    assert!(notes[1].as_str().unwrap().contains("to stay within"));
}

fn numbered_lines(count: usize) -> String {
    (1..=count).map(|n| format!("{n}\n")).collect()
}

#[tokio::test]
async fn script_read_head_lines_reports_the_lines_returned() {
    let result = make_server_read(numbered_lines(10).as_bytes(), |p| p.head_lines = Some(2)).await;
    let f = fields(&result);
    assert_eq!(f["text"], "1\n2\n");
    assert_eq!(f["start_line"], 1);
    assert_eq!(f["end_line"], 2);
}

#[tokio::test]
async fn script_read_tail_lines_reports_the_lines_returned() {
    let result = make_server_read(numbered_lines(10).as_bytes(), |p| p.tail_lines = Some(3)).await;
    let f = fields(&result);
    assert_eq!(f["text"], "8\n9\n10\n");
    assert_eq!(f["start_line"], 8);
    assert_eq!(f["end_line"], 10);
}
