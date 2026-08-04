use super::*;

struct ReplaceCase<'a> {
    file_name: &'a str,
    content: &'a str,
    pattern: &'a str,
    replacement: &'a str,
    replace_all: Option<bool>,
}

async fn run_replace(
    temp_dir: &TestDir,
    case: ReplaceCase<'_>,
) -> (PathBuf, Result<CallToolResult, ErrorData>) {
    let file_path = write_fixture(temp_dir, case.file_name, case.content);
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));
    let result = server
        .re_replace_impl(ReReplaceParams {
            path: path_string(&file_path),
            pattern: case.pattern.to_string(),
            replacement: case.replacement.to_string(),
            replace_all: case.replace_all,
        })
        .await;
    (file_path, result)
}

#[tokio::test]
async fn test_re_replace_basic() {
    let temp_dir = TestDir::new();
    let (file_path, result) = run_replace(
        &temp_dir,
        ReplaceCase {
            file_name: "re_basic.txt",
            content: "foo123\n",
            pattern: r"foo(\d+)",
            replacement: "bar$1",
            replace_all: None,
        },
    )
    .await;
    let result = result.unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "bar123\n");
}

#[tokio::test]
async fn test_re_replace_all() {
    let temp_dir = TestDir::new();
    let (file_path, result) = run_replace(
        &temp_dir,
        ReplaceCase {
            file_name: "re_all.txt",
            content: "foo1 foo2 foo3\n",
            pattern: r"foo(\d+)",
            replacement: "bar$1",
            replace_all: Some(true),
        },
    )
    .await;
    let result = result.unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "bar1 bar2 bar3\n"
    );
}

#[tokio::test]
async fn test_re_replace_no_match() {
    let temp_dir = TestDir::new();
    let (_, result) = run_replace(
        &temp_dir,
        ReplaceCase {
            file_name: "re_no_match.txt",
            content: "alpha\nbeta\n",
            pattern: "foo",
            replacement: "bar",
            replace_all: None,
        },
    )
    .await;
    let result = result.unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("did not match"));
}

#[tokio::test]
async fn test_re_replace_multiple_no_flag() {
    let temp_dir = TestDir::new();
    let (_, result) = run_replace(
        &temp_dir,
        ReplaceCase {
            file_name: "re_multiple.txt",
            content: "foo1 foo2\n",
            pattern: r"foo(\d+)",
            replacement: "bar$1",
            replace_all: None,
        },
    )
    .await;
    let result = result.unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("replace_all"));
}

#[tokio::test]
async fn test_re_replace_invalid_pattern() {
    let temp_dir = TestDir::new();
    let (_, result) = run_replace(
        &temp_dir,
        ReplaceCase {
            file_name: "re_invalid.txt",
            content: "foo1\n",
            pattern: "(",
            replacement: "bar",
            replace_all: None,
        },
    )
    .await;
    let err = result.unwrap_err();

    assert!(err.message.contains("invalid regex pattern"));
}
