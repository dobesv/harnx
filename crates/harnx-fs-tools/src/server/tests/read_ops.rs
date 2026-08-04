use super::*;

#[tokio::test]
async fn test_read_file_with_offset_limit() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("offset.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(2),
            limit: Some(2),
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("2: two"));
    assert!(text.contains("3: three"));
    assert!(text.contains("Use offset=4 to continue"));
}

#[tokio::test]
async fn test_read_file_with_grep() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("grep.txt");
    std::fs::write(&file_path, "alpha\nmatch-one\nbeta\nmatch-two\n").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: None,
            grep: Some("match".to_string()),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("2: match-one"));
    assert!(text.contains("4: match-two"));
    assert!(!text.contains("1: alpha"));
}

#[tokio::test]
async fn test_read_file_with_tail() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("tail.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: Some(2),
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("3: three"));
    assert!(text.contains("4: four"));
    assert!(text.contains("showing last 2 of 4 matching lines"));
}

/// Read `content` with combined `offset`+`tail` and return the rendered text.
async fn read_offset_tail(content: &str, offset: usize, tail: usize) -> String {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("offset_tail.txt");
    std::fs::write(&file_path, content).unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(offset),
            limit: None,
            tail: Some(tail),
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
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
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("offset_tail_eof.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\n").unwrap();
    let server = make_server(temp_dir.path());

    // offset one past EOF (total=3, offset=4) with tail must error, matching
    // the non-tail path.
    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(4),
            limit: None,
            tail: Some(2),
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await;

    let message = result.expect_err("expected error").message;
    assert!(
        message.contains("beyond end of result set"),
        "got: {message}"
    );
}

#[tokio::test]
async fn test_read_file_offset_zero_rejected() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("offset_zero.txt");
    std::fs::write(&file_path, "one\ntwo\n").unwrap();
    let server = make_server(temp_dir.path());

    // Explicit offset=0 is invalid (offset is 1-indexed), matching
    // read_exec_log's contract rather than silently coercing to 1.
    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(0),
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await;

    let message = result.expect_err("expected error").message;
    assert!(message.contains("offset must be >= 1"), "got: {message}");
}

#[tokio::test]
async fn test_read_file_binary_detection() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("binary.bin");
    std::fs::write(&file_path, b"hello\0world").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("appears to be a binary file"));
}

#[tokio::test]
async fn test_read_file_image_png() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("test.png");
    std::fs::write(&file_path, b"\x89PNG\r\n\x1a\n...fake...").unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
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
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("big.jpg");
    // > 5MB
    let big_data = vec![0xFF, 0xD8, 0xFF, 0x00];
    let mut file_data = big_data.clone();
    file_data.resize(5 * 1024 * 1024 + 10, 0);
    std::fs::write(&file_path, &file_data).unwrap();
    let server = make_server(temp_dir.path());

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
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
