use super::*;

#[tokio::test]
async fn test_insert_prepend() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("prepend.txt");
    std::fs::write(
        &file_path,
        "beta
gamma
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(0),
            insert_text: "alpha
"
            .to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha
beta
gamma
"
    );
}

#[tokio::test]
async fn test_insert_append() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("append.txt");
    std::fs::write(
        &file_path,
        "alpha
beta
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(2),
            insert_text: "gamma
"
            .to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha
beta
gamma
"
    );
}

#[tokio::test]
async fn test_insert_append_omit_line() {
    // Omitting insert_line entirely should append to end of file
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("append_omit.txt");
    std::fs::write(&file_path, "alpha\nbeta\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: None,
            insert_text: "gamma\n".to_string(),
            column: None,
        })
        .await
        .unwrap();

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
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("append_col.txt");
    std::fs::write(&file_path, "alpha\nbeta\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: None,
            insert_text: "gamma\n".to_string(),
            column: Some(3), // would have inserted into last line without the fix
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "alpha\nbeta\ngamma\n"
    );
}

#[tokio::test]
async fn test_insert_middle() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("middle.txt");
    std::fs::write(
        &file_path,
        "one
two
three
four
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(2),
            insert_text: "between
"
            .to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "one
two
between
three
four
"
    );
}

#[tokio::test]
async fn test_insert_column() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("column.txt");
    std::fs::write(
        &file_path,
        "abcd
xyz
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(1),
            insert_text: "-MID-".to_string(),
            column: Some(5),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "abcd-MID-
xyz
"
    );
}

#[tokio::test]
async fn test_insert_column_utf8_boundary() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("utf8_boundary.txt");
    std::fs::write(
        &file_path, "🦀abc
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let ok_result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(1),
            insert_text: "X".to_string(),
            column: Some(5),
        })
        .await
        .unwrap();

    assert_eq!(ok_result.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "🦀Xabc
"
    );

    std::fs::write(
        &file_path, "🦀abc
",
    )
    .unwrap();

    let bad_result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(1),
            insert_text: "X".to_string(),
            column: Some(2),
        })
        .await
        .unwrap();

    assert_eq!(bad_result.is_error, Some(true));
    assert!(text_content(&bad_result).contains("UTF-8 character boundary"));
}

#[tokio::test]
async fn test_insert_line_out_of_range() {
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("line_oob.txt");
    std::fs::write(
        &file_path,
        "alpha
beta
",
    )
    .unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(3),
            insert_text: "gamma
"
            .to_string(),
            column: None,
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_insert_column_out_of_range() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(
        &temp_dir,
        "column_oob.txt",
        "abcd
",
    );
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .insert_impl(InsertParams {
            path: path_string(&file_path),
            insert_line: Some(1),
            insert_text: "X".to_string(),
            column: Some(6),
        })
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
}
