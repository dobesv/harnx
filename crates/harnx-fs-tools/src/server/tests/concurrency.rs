use super::*;

#[tokio::test]
async fn test_insert_impl_serializes_concurrent_same_file_updates() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, "concurrent_insert.txt", "start\n");
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let task_count = 32usize;
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..task_count {
        let server = server.clone();
        let path = path_string(&file_path);
        tasks.spawn(async move {
            server
                .insert_impl(InsertParams {
                    path,
                    insert_line: None,
                    insert_text: format!("task-{index}\n"),
                    column: None,
                })
                .await
        });
    }

    while let Some(result) = tasks.join_next().await {
        let result = result.unwrap().unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    let content = std::fs::read_to_string(&file_path).unwrap();
    let lines = content.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), task_count + 1);
    assert_eq!(lines[0], "start");

    for index in 0..task_count {
        assert!(
            lines.contains(&format!("task-{index}").as_str()),
            "missing task-{index} in final file: {content:?}"
        );
    }
}

#[tokio::test]
async fn test_repo_lock_for_paths_in_same_repo_share_lock() {
    let temp_dir = TestDir::new();
    init_git_repo(temp_dir.path());
    let repo_root = temp_dir.path().canonicalize().unwrap();
    let file_a = repo_root.join("a.txt");
    std::fs::write(&file_a, "a\n").unwrap();
    let nested_dir = repo_root.join("nested");
    std::fs::create_dir_all(&nested_dir).unwrap();
    let file_b = repo_root.join("nested/b.txt");
    std::fs::write(&file_b, "b\n").unwrap();
    let server = FsServer::new(rwx_allowlist([repo_root.clone()]));

    let repo_guard = server.repo_write_guard_for_path(&file_a).await;
    let same_repo_try = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        server.repo_read_guard_for_path(&file_b),
    )
    .await;

    assert!(
        same_repo_try.is_err(),
        "same repo lock should block readers while write held"
    );
    drop(repo_guard);

    server.repo_read_guard_for_path(&file_b).await;
}

/// Gated to Unix because `std::env::temp_dir()` on Windows runners
/// yields an 8.3 short-name path (`C:\\Users\\RUNNER~1\\...`); the
/// canonicalize-then-`gix::open` flow inside `HistoryManager::new`
/// then fails to register the repo, leaving the production code
/// without a base to diff against. That's a pre-existing Windows
/// limitation in `harnx-mcp-history`, not something this regression
/// test introduces.
#[cfg(unix)]
#[tokio::test]
async fn test_rollback_excludes_concurrent_edits_in_same_repo() {
    let temp_dir = TestDir::new();
    init_git_repo(temp_dir.path());
    let repo_root = temp_dir.path().canonicalize().unwrap();

    let tracked_path = repo_root.join("tracked.txt");
    std::fs::write(&tracked_path, "tracked-v1\n").unwrap();
    let server = FsServer::new(rwx_allowlist([repo_root.clone()]));

    let tracked_before = server
        .snapshot_before(&tracked_path, "before tracked change")
        .await
        .unwrap();
    std::fs::write(&tracked_path, "tracked-v2\n").unwrap();
    let rollback_diff = server
        .snapshot_after_diff(&tracked_path, Some(tracked_before), "after tracked change")
        .await
        .unwrap();
    let rollback_commit = rollback_diff
        .lines()
        .next()
        .unwrap()
        .strip_prefix("commit ")
        .unwrap()
        .to_string();

    let repo_guard = server.repo_write_guard_for_path(&tracked_path).await;
    let edit_server = server.clone();
    let edit_path = path_string(&repo_root.join("other.txt"));
    let blocked_edit = tokio::spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            edit_server.write_file_impl(WriteFileParams {
                path: edit_path,
                content: "blocked\n".to_string(),
            }),
        )
        .await
    });

    assert!(blocked_edit.await.unwrap().is_err());
    drop(repo_guard);

    let rollback_server = server.clone();
    let rollback_path = path_string(&repo_root);
    let rollback_task = tokio::spawn(async move {
        rollback_server
            .rollback_file_impl(RollbackParams {
                commit_id: rollback_commit,
                repo_path: rollback_path,
            })
            .await
    });

    let rollback_result = rollback_task.await.unwrap().unwrap();
    assert_eq!(rollback_result.is_error, Some(false));
}
