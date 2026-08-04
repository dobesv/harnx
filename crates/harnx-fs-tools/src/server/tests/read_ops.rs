use super::*;

#[derive(Default)]
struct ReadSpec {
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
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: spec.offset,
            limit: spec.limit,
            tail: spec.tail,
            grep: spec.grep,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
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
