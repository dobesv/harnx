use super::*;

#[tokio::test]
async fn test_edit_file_unique_match() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("unique.txt");
    std::fs::write(&file_path, "alpha\nbeta\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .edit_file_impl(EditFileParams {
            path: path_string(&file_path),
            old_text: "beta".to_string(),
            new_text: "gamma".to_string(),
            replace_all: Some(false),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\ngamma\n"
    );
}

#[tokio::test]
async fn test_edit_file_multiple_matches() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("multiple.txt");
    std::fs::write(&file_path, "value\nvalue\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .edit_file_impl(EditFileParams {
            path: path_string(&file_path),
            old_text: "value".to_string(),
            new_text: "updated".to_string(),
            replace_all: Some(false),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("Found 2 matches"));
}
