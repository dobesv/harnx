use super::*;

#[tokio::test]
async fn write_bytes_preserves_non_utf8_bytes() {
    let dir = TestDir::new();
    let path = fixture_path(&dir, "nested/image.png");
    let bytes = b"\x89PNG\r\n\x1a\n\xff\xff";

    let result = make_server(dir.path())
        .write_bytes_impl(&path_string(&path), bytes)
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        text_content(&result),
        format!(
            "Wrote binary file, {} bytes written to {}",
            bytes.len(),
            path.display()
        )
    );
    assert_eq!(result.content.len(), 1);
    assert_binary_output_is_safe(&result);
}

#[tokio::test]
async fn write_bytes_preserves_utf8_summaries() {
    let dir = TestDir::new();
    let path = fixture_path(&dir, "nested/notes.txt");
    let server = make_server(dir.path());

    for text in ["alpha\r\nβeta\n", "single line", ""] {
        let result = server
            .write_bytes_impl(&path_string(&path), text.as_bytes())
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
        assert_eq!(
            text_content(&result),
            format!(
                "Wrote {} ({} lines) to {}",
                format_size(text.len()),
                text.lines().count(),
                path.display()
            )
        );
    }
}

#[tokio::test]
async fn write_bytes_denies_paths_outside_allowlist() {
    let allowed_dir = TestDir::new();
    let denied_dir = TestDir::new();
    let path = fixture_path(&denied_dir, "nested/image.png");

    let error = make_server(allowed_dir.path())
        .write_bytes_impl(&path_string(&path), b"\xff")
        .await
        .unwrap_err();

    assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(!path.parent().unwrap().exists());
}

#[tokio::test]
async fn write_bytes_rejects_oversize_without_touching_destination() {
    let dir = TestDir::new();
    let existing = write_fixture(&dir, "existing.png", b"\xfforiginal");
    let missing = fixture_path(&dir, "nested/image.png");
    let server = make_server(dir.path());
    let bytes = vec![0xff; WRITE_MAX_BYTES + 1];

    for path in [&existing, &missing] {
        let result = server
            .write_bytes_impl(&path_string(path), &bytes)
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            text_content(&result),
            format!(
                "Content too large ({}, max {})",
                format_size(bytes.len()),
                format_size(WRITE_MAX_BYTES)
            )
        );
    }
    assert_eq!(std::fs::read(existing).unwrap(), b"\xfforiginal");
    assert!(!missing.parent().unwrap().exists());
}

pub(super) fn assert_binary_output_is_safe(result: &CallToolResult) {
    for content in &result.content {
        let text = &content.as_text().expect("text summary").text;
        assert!(!text.contains('\u{fffd}'), "lossy output: {text:?}");
        assert!(!text.contains("PNG"), "binary payload in output: {text:?}");
        assert!(!text.contains("lines"), "binary line count: {text:?}");
        assert!(
            text.chars().all(|c| !c.is_control() || c == '\n'),
            "binary control characters in output: {text:?}"
        );
    }
}
