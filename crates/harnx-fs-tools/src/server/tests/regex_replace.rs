use super::*;

#[tokio::test]
async fn test_re_replace_basic() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_basic.txt",
        "foo123
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: r"foo(\d+)".to_string(),
            replacement: "bar$1".to_string(),
            replace_all: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "bar123
"
    );
}

#[tokio::test]
async fn test_re_replace_all() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_all.txt",
        "foo1 foo2 foo3
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: r"foo(\d+)".to_string(),
            replacement: "bar$1".to_string(),
            replace_all: Some(true),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "bar1 bar2 bar3
"
    );
}

#[tokio::test]
async fn test_re_replace_no_match() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_no_match.txt",
        "alpha
beta
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: "foo".to_string(),
            replacement: "bar".to_string(),
            replace_all: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("did not match"));
}

#[tokio::test]
async fn test_re_replace_multiple_no_flag() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_multiple.txt",
        "foo1 foo2
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: r"foo(\d+)".to_string(),
            replacement: "bar$1".to_string(),
            replace_all: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("replace_all"));
}

#[tokio::test]
async fn test_re_replace_invalid_pattern() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "re_invalid.txt",
        "foo1
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let err = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: "(".to_string(),
            replacement: "bar".to_string(),
            replace_all: None,
        })
        .await
        .unwrap_err();

    assert!(err.message.contains("invalid regex pattern"));
}
