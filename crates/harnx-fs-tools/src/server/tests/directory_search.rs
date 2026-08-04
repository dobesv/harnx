use super::*;

#[tokio::test]
async fn test_list_directory_flat() {
    let temp_dir = TestDir::new();
    std::fs::create_dir_all(temp_dir.path().join("nested")).unwrap();
    std::fs::write(temp_dir.path().join("root.txt"), "root").unwrap();
    std::fs::write(temp_dir.path().join("nested").join("child.txt"), "child").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .list_directory_impl(ListDirectoryParams {
            path: temp_dir.path().to_string_lossy().to_string(),
            recursive: Some(false),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("nested/"));
    assert!(text.contains("root.txt"));
    assert!(!text.contains("child.txt"));
}

#[tokio::test]
async fn test_search_files_basic() {
    let temp_dir = TestDir::new();
    std::fs::write(temp_dir.path().join("one.txt"), "alpha\nneedle\nomega\n").unwrap();
    std::fs::write(temp_dir.path().join("two.txt"), "nothing here\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .search_files_impl(SearchFilesParams {
            pattern: "needle".to_string(),
            path: Some(temp_dir.path().to_string_lossy().to_string()),
            include: Some("*.txt".to_string()),
            context_lines: Some(0),
            ignore_case: Some(false),
            max_results: Some(10),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("one.txt:2: needle"));
    assert!(!text.contains("two.txt"));
}

// ── truncation-in-user-summary tests (issue #144) ──────────────────────

#[tokio::test]
async fn test_read_file_summary_limited_on_pagination() {
    // offset=1 limit=2 on a 4-line file → shows lines 1–2, more remain.
    // Summary must show the slice range and byte counts.
    let temp_dir = TestDir::new();
    let file_path = temp_dir.path().join("paginated.txt");
    std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .read_file_impl(ReadFileParams {
            path: path_string(&file_path),
            offset: Some(1),
            limit: Some(2),
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let summary = user_summary(&result);
    assert!(
        summary.contains("lines 1\u{2013}2 of 4"),
        "expected exact paginated range 'lines 1\u{2013}2 of 4' in summary, got: {summary:?}"
    );
}

#[tokio::test]
async fn test_list_directory_summary_not_limited_when_small() {
    let temp_dir = TestDir::new();
    for i in 0..3 {
        std::fs::write(temp_dir.path().join(format!("f{i}.txt")), "x").unwrap();
    }
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .list_directory_impl(ListDirectoryParams {
            path: temp_dir.path().to_string_lossy().to_string(),
            recursive: Some(false),
        })
        .await
        .unwrap();

    let summary = user_summary(&result);
    assert!(
        !summary.contains("limited"),
        "expected no 'limited' for small listing, got: {summary:?}"
    );
    assert!(
        summary.contains("Listed 3 entries"),
        "expected count in summary, got: {summary:?}"
    );
}

#[tokio::test]
async fn test_list_directory_summary_limited_when_over_default_limit() {
    // Create DEFAULT_LS_LIMIT + 1 files to trigger limit_reached.
    // Summary should show "Listed 500 of 501 entries in …".
    let temp_dir = TestDir::new();
    for i in 0..=DEFAULT_LS_LIMIT {
        std::fs::write(temp_dir.path().join(format!("f{i:04}.txt")), "x").unwrap();
    }
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .list_directory_impl(ListDirectoryParams {
            path: temp_dir.path().to_string_lossy().to_string(),
            recursive: Some(false),
        })
        .await
        .unwrap();

    let summary = user_summary(&result);
    // Should show "Listed 500 of 501 entries" — capped count + true total.
    assert!(
        summary.contains(&format!(
            "Listed {} of {} entries",
            DEFAULT_LS_LIMIT,
            DEFAULT_LS_LIMIT + 1
        )),
        "expected 'Listed N of M entries' in summary, got: {summary:?}"
    );
}

#[tokio::test]
async fn test_search_files_summary_variants() {
    struct Case {
        files: &'static [(&'static str, &'static str)],
        max_results: usize,
        check: fn(&str),
    }

    let cases: &[Case] = &[
        Case {
            files: &[
                ("match0.txt", "needle\n"),
                ("match1.txt", "needle\n"),
                ("match2.txt", "needle\n"),
            ],
            max_results: 1,
            check: |summary| {
                assert!(
                    summary.contains("1+"),
                    "expected '1+' in summary when max_results hit, got: {summary:?}"
                );
                assert!(
                    summary.contains("showing 1"),
                    "expected 'showing 1' in summary, got: {summary:?}"
                );
            },
        },
        Case {
            files: &[("one.txt", "needle\n")],
            max_results: 10,
            check: |summary| {
                assert!(
                    !summary.contains("limited"),
                    "expected no 'limited' when all results returned, got: {summary:?}"
                );
            },
        },
    ];

    for case in cases {
        let temp_dir = TestDir::new();
        for (name, content) in case.files {
            std::fs::write(temp_dir.path().join(name), content).unwrap();
        }
        let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

        let result = server
            .search_files_impl(SearchFilesParams {
                pattern: "needle".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                include: None,
                context_lines: Some(0),
                ignore_case: Some(false),
                max_results: Some(case.max_results),
            })
            .await
            .unwrap();

        (case.check)(user_summary(&result).as_str());
    }
}

#[tokio::test]
async fn test_find_files_basic() {
    // Regression: glob pattern must use '/' not MAIN_SEPARATOR —
    // the glob crate expects Unix separators on all platforms.
    let temp_dir = TestDir::new();
    std::fs::write(temp_dir.path().join("hello.txt"), "").unwrap();
    let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

    let result = server
        .find_files_impl(FindFilesParams {
            pattern: "*.txt".to_string(),
            path: Some(temp_dir.path().to_string_lossy().to_string()),
            max_results: Some(10),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("hello.txt"),
        "find_files should locate files on any platform, got: {text:?}"
    );
}

#[tokio::test]
async fn test_find_files_summary_variants() {
    struct Case {
        files: &'static [&'static str],
        max_results: usize,
        check: fn(&str),
    }

    let cases: &[Case] = &[
        Case {
            files: &["file0.txt", "file1.txt", "file2.txt"],
            max_results: 1,
            check: |summary| {
                assert!(
                    summary.contains("1+"),
                    "expected '1+' in find_files summary when max_results hit, got: {summary:?}"
                );
                assert!(
                    summary.contains("showing 1"),
                    "expected 'showing 1' in find_files summary, got: {summary:?}"
                );
            },
        },
        Case {
            files: &["only.txt"],
            max_results: 10,
            check: |summary| {
                assert!(
                    !summary.contains("limited"),
                    "expected no 'limited' when all files returned, got: {summary:?}"
                );
            },
        },
    ];

    for case in cases {
        let temp_dir = TestDir::new();
        for name in case.files {
            std::fs::write(temp_dir.path().join(name), "").unwrap();
        }
        let server = FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]));

        let result = server
            .find_files_impl(FindFilesParams {
                pattern: "*.txt".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                max_results: Some(case.max_results),
            })
            .await
            .unwrap();

        (case.check)(user_summary(&result).as_str());
    }
}
