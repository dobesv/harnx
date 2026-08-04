use super::*;

struct InsertSpec<'a> {
    line: Option<usize>,
    text: &'a str,
    column: Option<usize>,
}

fn insert_fixture(file_name: &str, content: &str) -> (TestDir, PathBuf, FsServer) {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, file_name, content);
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));
    (temp_dir, file_path, server)
}

async fn run_insert(server: &FsServer, file_path: &Path, spec: InsertSpec<'_>) -> CallToolResult {
    server
        .insert_impl(InsertParams {
            path: path_string(file_path),
            insert_line: spec.line,
            insert_text: spec.text.to_string(),
            column: spec.column,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn test_insert_prepend() {
    let (_temp, file_path, server) = insert_fixture("prepend.txt", "beta\ngamma\n");
    let result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: Some(0),
            text: "alpha\n",
            column: None,
        },
    )
    .await;

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn test_insert_append() {
    let (_temp, file_path, server) = insert_fixture("append.txt", "alpha\nbeta\n");
    let result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: Some(2),
            text: "gamma\n",
            column: None,
        },
    )
    .await;

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn test_insert_append_omit_line() {
    // Omitting insert_line entirely should append to end of file
    let (_temp, file_path, server) = insert_fixture("append_omit.txt", "alpha\nbeta\n");
    let result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: None,
            text: "gamma\n",
            column: None,
        },
    )
    .await;

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn test_insert_append_omit_line_ignores_column() {
    // When insert_line is omitted, a supplied column must NOT trigger
    // mid-line insertion — it must still append at EOF.
    let (_temp, file_path, server) = insert_fixture("append_col.txt", "alpha\nbeta\n");
    let result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: None,
            text: "gamma\n",
            column: Some(3),
        },
    )
    .await;

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn test_insert_middle() {
    let (_temp, file_path, server) = insert_fixture("middle.txt", "one\ntwo\nthree\nfour\n");
    let result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: Some(2),
            text: "between\n",
            column: None,
        },
    )
    .await;

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "one\ntwo\nbetween\nthree\nfour\n"
    );
}

#[tokio::test]
async fn test_insert_column() {
    let (_temp, file_path, server) = insert_fixture("column.txt", "abcd\nxyz\n");
    let result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: Some(1),
            text: "-MID-",
            column: Some(5),
        },
    )
    .await;

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "abcd-MID-\nxyz\n"
    );
}

#[tokio::test]
async fn test_insert_column_utf8_boundary() {
    let (_temp, file_path, server) = insert_fixture("utf8_boundary.txt", "🦀abc\n");
    let ok_result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: Some(1),
            text: "X",
            column: Some(5),
        },
    )
    .await;

    assert_eq!(ok_result.is_error, Some(false));
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "🦀Xabc\n");

    std::fs::write(&file_path, "🦀abc\n").unwrap();
    let bad_result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: Some(1),
            text: "X",
            column: Some(2),
        },
    )
    .await;

    assert_eq!(bad_result.is_error, Some(true));
    assert!(text_content(&bad_result).contains("UTF-8 character boundary"));
}

#[tokio::test]
async fn test_insert_line_out_of_range() {
    let (_temp, file_path, server) = insert_fixture("line_oob.txt", "alpha\nbeta\n");
    let result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: Some(3),
            text: "gamma\n",
            column: None,
        },
    )
    .await;

    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_insert_column_out_of_range() {
    let (_temp, file_path, server) = insert_fixture("column_oob.txt", "abcd\n");
    let result = run_insert(
        &server,
        &file_path,
        InsertSpec {
            line: Some(1),
            text: "X",
            column: Some(6),
        },
    )
    .await;

    assert_eq!(result.is_error, Some(true));
}
