use super::*;

async fn list_directory(temp_dir: &TestDir) -> CallToolResult {
    FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]))
        .list_directory_impl(
            ListDirectoryParams {
                path: temp_dir.path().to_string_lossy().to_string(),
                recursive: Some(false),
            },
            ResultStyle::Model,
        )
        .await
        .unwrap()
}

struct SearchSpec<'a> {
    include: Option<&'a str>,
    max_results: usize,
}

async fn search_files(temp_dir: &TestDir, spec: SearchSpec<'_>) -> CallToolResult {
    FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]))
        .search_files_impl(
            SearchFilesParams {
                pattern: "needle".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                include: spec.include.map(str::to_string),
                context_lines: Some(0),
                ignore_case: Some(false),
                max_results: Some(spec.max_results),
            },
            ResultStyle::Model,
        )
        .await
        .unwrap()
}

async fn find_files(temp_dir: &TestDir, max_results: usize) -> CallToolResult {
    FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]))
        .find_files_impl(
            FindFilesParams {
                pattern: "*.txt".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                max_results: Some(max_results),
            },
            ResultStyle::Model,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn test_list_directory_flat() {
    let temp_dir = TestDir::new();
    std::fs::create_dir_all(temp_dir.path().join("nested")).unwrap();
    std::fs::write(temp_dir.path().join("root.txt"), "root").unwrap();
    std::fs::write(temp_dir.path().join("nested").join("child.txt"), "child").unwrap();
    let result = list_directory(&temp_dir).await;

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
    let result = search_files(
        &temp_dir,
        SearchSpec {
            include: Some("*.txt"),
            max_results: 10,
        },
    )
    .await;

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
        .read_file_impl(
            ReadFileParams {
                path: path_string(&file_path),
                offset: Some(1),
                limit: Some(2),
                tail: None,
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                line_numbers: None,
            },
            ResultStyle::Model,
        )
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
    let result = list_directory(&temp_dir).await;

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
    let result = list_directory(&temp_dir).await;

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
        let result = search_files(
            &temp_dir,
            SearchSpec {
                include: None,
                max_results: case.max_results,
            },
        )
        .await;

        (case.check)(user_summary(&result).as_str());
    }
}

#[tokio::test]
async fn test_find_files_basic() {
    // Regression: glob pattern must use '/' not MAIN_SEPARATOR —
    // the glob crate expects Unix separators on all platforms.
    let temp_dir = TestDir::new();
    std::fs::write(temp_dir.path().join("hello.txt"), "").unwrap();
    let result = find_files(&temp_dir, 10).await;

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
        let result = find_files(&temp_dir, case.max_results).await;

        (case.check)(user_summary(&result).as_str());
    }
}

async fn grep_with_context(temp_dir: &TestDir, max_results: usize) -> CallToolResult {
    FsServer::new(rwx_allowlist([temp_dir.path().to_path_buf()]))
        .search_files_impl(
            SearchFilesParams {
                pattern: "needle".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                include: None,
                context_lines: Some(1),
                ignore_case: Some(false),
                max_results: Some(max_results),
            },
            ResultStyle::Model,
        )
        .await
        .unwrap()
}

fn two_block_fixture() -> TestDir {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "a.txt", "needle one\nx\ny\nz\nneedle two\nw\n");
    temp_dir
}

#[tokio::test]
async fn model_grep_output_is_unchanged_with_context() {
    let temp_dir = two_block_fixture();
    let result = grep_with_context(&temp_dir, 10).await;
    assert_eq!(
        text_content(&result),
        "a.txt:1: needle one\na.txt-2- x\n--\na.txt-4- z\na.txt:5: needle two\na.txt-6- w"
    );
}

#[tokio::test]
async fn model_grep_output_is_unchanged_at_the_result_limit() {
    let temp_dir = two_block_fixture();
    let result = grep_with_context(&temp_dir, 3).await;
    assert_eq!(
        text_content(&result),
        "a.txt:1: needle one\na.txt-2- x\n--\n\n[results limited to 3 matches. Refine the pattern for more specific results]"
    );
}

async fn script_call(temp_dir: &TestDir, tool: &str, args: Value) -> Value {
    let mut args = args;
    args["path"] = Value::String(path_string(temp_dir.path()));
    let result = make_server(temp_dir.path())
        .invoke_tool_value(tool, args, ResultStyle::Script)
        .await
        .unwrap();
    result.structured_content.expect("script style has fields")
}

#[tokio::test]
async fn script_ls_lists_entries_and_empty_directory_is_empty() {
    let temp_dir = TestDir::new();
    let fields = script_call(&temp_dir, "ls", json!({})).await;
    assert_eq!(fields["entries"], json!([]));
    assert_eq!(fields["notes"], json!([]));

    std::fs::create_dir_all(temp_dir.path().join("nested")).unwrap();
    write_fixture(&temp_dir, "root.txt", "x");
    let fields = script_call(&temp_dir, "ls", json!({})).await;
    assert_eq!(fields["entries"], json!(["nested/", "root.txt"]));
}

#[tokio::test]
async fn script_ls_reports_the_entry_cap_in_notes() {
    let temp_dir = TestDir::new();
    for i in 0..=DEFAULT_LS_LIMIT {
        write_fixture(&temp_dir, &format!("f{i:04}.txt"), "x");
    }
    let fields = script_call(&temp_dir, "ls", json!({})).await;
    assert_eq!(
        fields["entries"].as_array().unwrap().len(),
        DEFAULT_LS_LIMIT
    );
    assert_eq!(
        fields["notes"],
        json!([format!(
            "stopped at {DEFAULT_LS_LIMIT} entries. Use fs_find for a targeted search"
        )])
    );
}

#[tokio::test]
async fn script_find_lists_paths_and_no_hit_is_empty() {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "b.txt", "");
    write_fixture(&temp_dir, "a.txt", "");
    let fields = script_call(&temp_dir, "find", json!({"pattern": "*.txt"})).await;
    let dir = path_string(temp_dir.path());
    assert_eq!(
        fields["paths"],
        json!([format!("{dir}/a.txt"), format!("{dir}/b.txt")])
    );
    assert_eq!(fields["notes"], json!([]));
    let fields = script_call(&temp_dir, "find", json!({"pattern": "*.rs"})).await;
    assert_eq!(fields["paths"], json!([]));
}

#[tokio::test]
async fn script_grep_returns_rows_and_exit_zero() {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "one.txt", "alpha\nneedle\n");
    let fields = script_call(&temp_dir, "grep", json!({"pattern": "needle"})).await;
    let dir = path_string(temp_dir.path());
    assert_eq!(
        fields["matches"],
        json!([{"path": format!("{dir}/one.txt"), "line": 2, "text": "needle"}])
    );
    assert_eq!(fields["exit_code"], json!(0));
}

#[tokio::test]
async fn script_grep_without_a_match_has_exit_code_one_and_no_notes() {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "one.txt", "alpha\n");
    let fields = script_call(&temp_dir, "grep", json!({"pattern": "needle"})).await;
    assert_eq!(fields["matches"], json!([]));
    assert_eq!(fields["notes"], json!([]));
    assert_eq!(fields["exit_code"], json!(1));
}

#[tokio::test]
async fn script_grep_marks_context_rows_and_drops_block_breaks() {
    let temp_dir = two_block_fixture();
    let fields = script_call(
        &temp_dir,
        "grep",
        json!({"pattern": "needle", "context_lines": 1}),
    )
    .await;
    let rows = fields["matches"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    let path = format!("{}/a.txt", path_string(temp_dir.path()));
    assert_eq!(
        rows[0],
        json!({"path": path, "line": 1, "text": "needle one"})
    );
    assert_eq!(
        rows[1],
        json!({"path": path, "line": 2, "text": "x", "context": true})
    );
    assert_eq!(rows[3]["line"], json!(5));
    assert!(rows[3].get("context").is_none());
}

#[tokio::test]
async fn script_grep_reports_the_match_cap_in_notes() {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "one.txt", "needle\nneedle\n");
    let fields = script_call(
        &temp_dir,
        "grep",
        json!({"pattern": "needle", "max_results": 1}),
    )
    .await;
    assert_eq!(fields["matches"].as_array().unwrap().len(), 1);
    assert_eq!(
        fields["notes"],
        json!(["stopped at 1 matches. Refine the pattern for more specific results"])
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn script_grep_keeps_a_path_with_spaces_and_colons_as_one_field() {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "my file: v2.txt", "needle\n");
    let fields = script_call(&temp_dir, "grep", json!({"pattern": "needle"})).await;
    let expected = format!("{}/my file: v2.txt", path_string(temp_dir.path()));
    assert_eq!(fields["matches"][0]["path"], json!(expected));
}

// With a zero limit the search still finds one hit before the cut, so the
// direct-call output is the empty listing plus the truncation notice.
#[tokio::test]
async fn model_grep_output_is_unchanged_with_zero_max_results() {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "one.txt", "needle\n");
    let result = search_files(
        &temp_dir,
        SearchSpec {
            include: None,
            max_results: 0,
        },
    )
    .await;
    assert_eq!(
        text_content(&result),
        "\n\n[results limited to 0 matches. Refine the pattern for more specific results]"
    );
}

#[tokio::test]
async fn model_find_output_is_unchanged_with_zero_max_results() {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "hello.txt", "");
    let result = find_files(&temp_dir, 0).await;
    assert_eq!(
        text_content(&result),
        "\n\n[Truncated at 0 results. Use a more specific pattern.]"
    );
}

#[tokio::test]
async fn script_grep_exit_code_ignores_a_zero_result_limit() {
    let temp_dir = TestDir::new();
    write_fixture(&temp_dir, "one.txt", "needle\n");
    let fields = script_call(
        &temp_dir,
        "grep",
        json!({"pattern": "needle", "max_results": 0}),
    )
    .await;
    assert_eq!(fields["matches"], json!([]));
    assert_eq!(fields["exit_code"], json!(0));
}

fn subdir_fixture() -> TestDir {
    let temp_dir = TestDir::new();
    std::fs::create_dir_all(temp_dir.path().join("sub")).unwrap();
    write_fixture(&temp_dir, "sub/lib.rs", "needle here\n");
    temp_dir
}

async fn script_value(server: &FsServer, tool: &str, args: Value) -> Value {
    server
        .invoke_tool_value(tool, args, ResultStyle::Script)
        .await
        .unwrap()
        .structured_content
        .unwrap()
}

#[tokio::test]
async fn script_find_paths_can_be_read_back() {
    let temp_dir = subdir_fixture();
    let server = make_server(temp_dir.path());
    let dir = path_string(&temp_dir.path().join("sub"));
    let fields = script_value(&server, "find", json!({"pattern": "*.rs", "path": dir})).await;
    let path = fields["paths"][0].as_str().unwrap().to_string();
    assert_eq!(path, format!("{dir}/lib.rs"));
    let read = script_value(&server, "read", json!({"path": path})).await;
    assert_eq!(read["text"], json!("needle here\n"));
}

#[tokio::test]
async fn script_grep_paths_can_be_read_back() {
    let temp_dir = subdir_fixture();
    let server = make_server(temp_dir.path());
    let dir = path_string(&temp_dir.path().join("sub"));
    let fields = script_value(&server, "grep", json!({"pattern": "needle", "path": dir})).await;
    let path = fields["matches"][0]["path"].as_str().unwrap().to_string();
    assert_eq!(path, format!("{dir}/lib.rs"));
    let read = script_value(&server, "read", json!({"path": path})).await;
    assert_eq!(read["text"], json!("needle here\n"));
}

#[tokio::test]
async fn script_find_prefix_does_not_double_a_trailing_slash() {
    let temp_dir = subdir_fixture();
    let server = make_server(temp_dir.path());
    let dir = format!("{}/", path_string(&temp_dir.path().join("sub")));
    let fields = script_value(&server, "find", json!({"pattern": "*.rs", "path": dir})).await;
    assert_eq!(fields["paths"], json!([format!("{dir}lib.rs")]));
}

#[tokio::test]
async fn script_find_without_a_path_prefixes_a_directory_that_is_not_the_cwd() {
    let temp_dir = subdir_fixture();
    let server = make_server(temp_dir.path());
    let fields = script_value(&server, "find", json!({"pattern": "sub/*.rs"})).await;
    let path = fields["paths"][0].as_str().unwrap();
    // The prefix is the allowlist's stored form of the directory, which may
    // be a short 8.3 path on Windows, so compare what both resolve to.
    let expected = temp_dir.path().join("sub").join("lib.rs");
    assert_eq!(
        Path::new(path).canonicalize().unwrap(),
        expected.canonicalize().unwrap(),
        "got {path:?}"
    );
    assert!(Path::new(path).is_absolute(), "{path:?}");
}

async fn grep_file_arg(temp_dir: &TestDir, given: &str, style: ResultStyle) -> CallToolResult {
    let params = SearchFilesParams {
        pattern: "needle".to_string(),
        path: Some(given.to_string()),
        include: None,
        context_lines: Some(0),
        ignore_case: Some(false),
        max_results: None,
    };
    make_server(temp_dir.path())
        .search_files_impl(params, style)
        .await
        .unwrap()
}

#[tokio::test]
async fn grep_searches_a_single_file_path() {
    let temp_dir = TestDir::new();
    let file = write_fixture(&temp_dir, "one.txt", "x\nneedle here\n");
    let result = grep_file_arg(&temp_dir, &path_string(&file), ResultStyle::Model).await;
    let text = text_content(&result);
    assert!(text.contains("one.txt:2: needle here"), "{text}");
    assert!(!text.contains("No matches found"));
}

#[tokio::test]
async fn script_grep_on_a_file_prints_the_given_path() {
    let temp_dir = TestDir::new();
    let file = write_fixture(&temp_dir, "one.txt", "x\nneedle here\n");
    let given = path_string(&file);
    let result = grep_file_arg(&temp_dir, &given, ResultStyle::Script).await;
    let fields = result.structured_content.unwrap();
    assert_eq!(fields["matches"][0]["path"], json!(given));
    assert_eq!(fields["matches"][0]["line"], json!(2));
    assert_eq!(fields["exit_code"], json!(0));
}

#[tokio::test]
async fn script_grep_on_a_file_without_a_match_exits_one() {
    let temp_dir = TestDir::new();
    let file = write_fixture(&temp_dir, "one.txt", "nothing\n");
    let result = grep_file_arg(&temp_dir, &path_string(&file), ResultStyle::Script).await;
    let fields = result.structured_content.unwrap();
    assert_eq!(fields["matches"], json!([]));
    assert_eq!(fields["exit_code"], json!(1));
}
