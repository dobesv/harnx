use super::*;

#[cfg(unix)]
#[tokio::test]
async fn read_only_allowlist_reads_but_denies_writes() {
    let dir = TestDir::new();
    let file = dir.path().join("read-only.txt");
    std::fs::write(&file, "original").unwrap();
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_read(dir.path());
    let server = FsServer::new(allowlist);

    server
        .read_file_impl(ReadFileParams {
            path: path_string(&file),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .expect("read grant permits reads");

    let denied = server
        .write_file_impl(WriteFileParams {
            path: path_string(&file),
            content: "changed".into(),
        })
        .await
        .unwrap_err();
    assert!(denied.message.contains("filesystem writes are denied"));
}

#[tokio::test]
async fn rollback_requires_write_allowlist() {
    let dir = TestDir::new();
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_read(dir.path());
    let server = FsServer::new(allowlist);

    let denied = server
        .rollback_file_impl(RollbackParams {
            commit_id: "not-reached".into(),
            repo_path: path_string(dir.path()),
        })
        .await
        .unwrap_err();
    assert!(denied.message.contains("filesystem writes are denied"));
}

#[tokio::test]
async fn rwx_allowlist_reads_and_writes() {
    let dir = TestDir::new();
    let file = dir.path().join("writable.txt");
    std::fs::write(&file, "original").unwrap();
    let server = FsServer::new(rwx_allowlist([dir.path().to_path_buf()]));

    server
        .write_file_impl(WriteFileParams {
            path: path_string(&file),
            content: "changed".into(),
        })
        .await
        .expect("rwx grant permits writes");
    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .expect("rwx grant permits reads");
    assert!(text_content(&result).contains("changed"));
}

#[tokio::test]
async fn empty_allowlist_denies_default_search_path() {
    let server = FsServer::new(ResolvedAllowlist::new());
    let denied = server
        .find_files_impl(FindFilesParams {
            pattern: "**/*".into(),
            path: None,
            max_results: None,
        })
        .await
        .unwrap_err();
    assert!(denied
        .message
        .contains("No readable directories configured"));
}
