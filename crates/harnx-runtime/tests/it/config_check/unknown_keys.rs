//! Each file parsed with the type the runtime reads it into: parse errors
//! with their line, and every key that type ignores reported by its path.

use std::path::Path;

use harnx_runtime::config_check::{Code, Finding, Report, Severity};

use super::{check, write};

/// Check a directory holding `files`, each a relative path and its content.
async fn check_holding(files: &[(&str, &str)]) -> Report {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        write(dir.path(), path, content);
    }
    check(dir.path()).await
}

/// The only finding for a directory holding `files`, which must have `code`
/// and be in `file`.
async fn only_finding(files: &[(&str, &str)], code: Code, file: &str) -> Finding {
    let report = check_holding(files).await;
    assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    let finding = report.findings[0].clone();
    assert_eq!(finding.code, code, "{finding:#?}");
    assert_eq!(finding.severity, Severity::Error, "{finding:#?}");
    assert_eq!(
        finding.file.as_deref(),
        Some(Path::new(file)),
        "{finding:#?}"
    );
    finding
}

/// `files` hold one unknown key, `key` in `file`, and the message names it
/// and suggests `suggestion`.
async fn assert_unknown_key(files: &[(&str, &str)], file: &str, key: &str, suggestion: &str) {
    let finding = only_finding(files, Code::UnknownKey, file).await;
    assert_eq!(finding.key_path.as_deref(), Some(key), "{finding:#?}");
    let expected = format!("unknown key {key} (did you mean {suggestion}?)");
    assert_eq!(finding.message, expected);
}

/// `files` hold one parse error, in `file` at `line`.
async fn assert_parse_error_at(files: &[(&str, &str)], file: &str, line: usize) {
    let finding = only_finding(files, Code::ParseError, file).await;
    assert_eq!(finding.line, Some(line), "{finding:#?}");
}

#[tokio::test]
async fn retention_typo_is_reported_with_suggestion() {
    assert_unknown_key(
        &[("config.yaml", "cleanup_remote_session_days: 30\n")],
        "config.yaml",
        "cleanup_remote_session_days",
        "cleanup_remote_sessions_days",
    )
    .await;
}

#[tokio::test]
async fn nested_unknown_key_has_dotted_path() {
    assert_unknown_key(
        &[(
            "config.yaml",
            "stream: true\nloop_detection:\n  tool_call: false\n",
        )],
        "config.yaml",
        "loop_detection.tool_call",
        "tool_calls",
    )
    .await;
}

#[tokio::test]
async fn agent_front_matter_unknown_key() {
    assert_unknown_key(
        &[(
            "agents/a.md",
            "---\ndescription: helper\nuse_tool: fs_*\n---\nYou help.\n",
        )],
        "agents/a.md",
        "use_tool",
        "use_tools",
    )
    .await;
}

#[tokio::test]
async fn agent_front_matter_parse_error_line_counts_from_the_file_start() {
    assert_parse_error_at(
        &[(
            "agents/a.md",
            "---\ndescription: helper\ntemperature: warm\n---\nYou help.\n",
        )],
        "agents/a.md",
        3,
    )
    .await;
}

#[tokio::test]
async fn agent_option_field_path_has_no_marker_for_the_option() {
    assert_unknown_key(
        &[(
            "agents/a.md",
            "---\nloop_detection:\n  outptu: false\n---\nYou help.\n",
        )],
        "agents/a.md",
        "loop_detection.outptu",
        "output",
    )
    .await;
}

#[tokio::test]
async fn agent_list_item_path_has_its_index() {
    assert_unknown_key(
        &[(
            "agents/a.md",
            "---\nvariables:\n  - name: x\n    description: d\n    defualt: y\n---\nYou help.\n",
        )],
        "agents/a.md",
        "variables.0.defualt",
        "default",
    )
    .await;
}

#[tokio::test]
async fn client_unknown_key_is_reported() {
    assert_unknown_key(
        &[("clients/openai.yaml", "type: openai\napi_kee: sk-test\n")],
        "clients/openai.yaml",
        "api_kee",
        "api_key",
    )
    .await;
}

#[tokio::test]
async fn client_unknown_type_is_error() {
    let finding = only_finding(
        &[("clients/openai.yaml", "type: opneai\n")],
        Code::UnknownClientType,
        "clients/openai.yaml",
    )
    .await;

    assert!(finding.message.contains("opneai"), "{}", finding.message);
    assert!(
        finding.message.contains("openai-compatible"),
        "valid types are listed: {}",
        finding.message
    );
}

#[tokio::test]
async fn tool_server_unknown_key() {
    assert_unknown_key(
        &[(
            "tool_servers/x.yaml",
            "command: harnx-time-tools\nargz: []\n",
        )],
        "tool_servers/x.yaml",
        "argz",
        "args",
    )
    .await;
}

#[tokio::test]
async fn package_patch_unknown_key() {
    assert_unknown_key(
        &[
            ("packages/p/package.yaml", "name: p\n"),
            ("packages/p.patch.yaml", "agentz: []\n"),
        ],
        "packages/p.patch.yaml",
        "agentz",
        "agents",
    )
    .await;
}

#[tokio::test]
async fn parse_error_has_line() {
    assert_parse_error_at(
        &[(
            "config.yaml",
            "stream: true\nsave: false\nhighlight: [oops\n",
        )],
        "config.yaml",
        3,
    )
    .await;
}

#[tokio::test]
async fn type_error_has_line() {
    assert_parse_error_at(
        &[(
            "config.yaml",
            "stream: true\nsave: false\nhighlight: sometimes\n",
        )],
        "config.yaml",
        3,
    )
    .await;
}

#[tokio::test]
async fn access_rules_with_anchors_are_clean() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "config.yaml", "stream: true\n");
    write(dir.path(), "agents/sisyphus.md", "plans\n");
    write(dir.path(), "packages/coding/agents/coder.md", "codes\n");
    write(
        dir.path(),
        "access.yaml",
        "rules:\n\
         \x20 - agents: &core_agents\n\
         \x20     - sisyphus\n\
         \x20     - \"coding/*\"\n\
         \x20   scopes: [prompt]\n\
         \x20   users: &engineers\n\
         \x20     - \"alice@example.com\"\n\
         \x20 - agents: *core_agents\n\
         \x20   scopes: [prompt, admin]\n\
         \x20   users: *engineers\n",
    );

    let report = check(dir.path()).await;

    assert!(report.findings.is_empty(), "{:#?}", report.findings);
}

#[tokio::test]
async fn access_rules_semantic_error_is_reported() {
    only_finding(
        &[
            ("config.yaml", "stream: true\n"),
            (
                "access.yaml",
                "rules:\n  - agents: [sisyphus]\n    scopes: [prompt]\n",
            ),
        ],
        Code::ParseError,
        "access.yaml",
    )
    .await;
}

#[tokio::test]
async fn users_file_unknown_field_is_reported() {
    only_finding(
        &[
            ("config.yaml", "stream: true\n"),
            (
                "users.yaml",
                "- name: alice\n  identities: [alice@example.com]\n  colour: blue\n",
            ),
        ],
        Code::ParseError,
        "users.yaml",
    )
    .await;
}

#[tokio::test]
async fn shipped_package_metadata_fields_parse() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "packages/p/package.yaml",
        "name: p\n\
         version: v0.4.23\n\
         description: >\n  A package.\n\
         harnx_min_version: \"0.34.0\"\n\
         homepage: https://github.com/dobesv/harnx\n\
         license: MIT OR Apache-2.0\n",
    );

    let report = check(dir.path()).await;

    assert!(report.findings.is_empty(), "{:#?}", report.findings);
}

#[tokio::test]
async fn package_files_are_each_checked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "packages/p/package.yaml", "name: p\n");
    write(
        root,
        "packages/p/clients/c.yaml",
        "type: claude\napi_key: x\nmodles: []\n",
    );
    write(
        root,
        "packages/p/tool_servers/t.yaml",
        "command: t\nenabled: false\ndescripton: x\n",
    );
    write(
        root,
        "packages/p/agents/w.md",
        "---\nmodel_fallback: []\n---\nWork.\n",
    );
    write(root, "packages/p/bash_tools/b.yaml", "name: [\n");

    let report = check(root).await;

    let mut found: Vec<(Code, String)> = report
        .findings
        .iter()
        .map(|finding| (finding.code, finding.file_display().unwrap()))
        .collect();
    found.sort_by(|left, right| left.1.cmp(&right.1));
    assert_eq!(
        found,
        vec![
            (Code::UnknownKey, "packages/p/agents/w.md".to_string()),
            (Code::ParseError, "packages/p/bash_tools/b.yaml".to_string()),
            (Code::UnknownKey, "packages/p/clients/c.yaml".to_string()),
            (
                Code::UnknownKey,
                "packages/p/tool_servers/t.yaml".to_string()
            ),
        ],
        "{:#?}",
        report.findings
    );
    assert_eq!(report.summary.files_checked, 5);
}

#[tokio::test]
async fn nats_server_macro_and_models_override_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "config.yaml", "stream: true\n");
    write(
        root,
        "nats_servers/shared.yaml",
        "url: nats://example:4222\nreplica: 3\n",
    );
    write(root, "macros/m.yaml", "steps: [\".help\"]\nvariabels: []\n");
    write(
        root,
        "models-override.yaml",
        &format!(
            "version: {}\nlist: []\nextra: 1\n",
            env!("CARGO_PKG_VERSION")
        ),
    );

    let report = check(root).await;

    let mut found: Vec<(Code, String, Option<String>)> = report
        .findings
        .iter()
        .map(|finding| {
            (
                finding.code,
                finding.file_display().unwrap(),
                finding.key_path.clone(),
            )
        })
        .collect();
    found.sort_by(|left, right| left.1.cmp(&right.1));
    assert_eq!(
        found,
        vec![
            (
                Code::UnknownKey,
                "macros/m.yaml".to_string(),
                Some("variabels".to_string())
            ),
            (
                Code::UnknownKey,
                "models-override.yaml".to_string(),
                Some("extra".to_string())
            ),
            (
                Code::UnknownKey,
                "nats_servers/shared.yaml".to_string(),
                Some("replica".to_string())
            ),
        ],
        "{:#?}",
        report.findings
    );
}

#[tokio::test]
async fn a_broken_client_is_reported_once() {
    assert_parse_error_at(
        &[
            ("config.yaml", "stream: true\n"),
            ("clients/openai.yaml", "type: openai\nmodels: notalist\n"),
        ],
        "clients/openai.yaml",
        2,
    )
    .await;
}

#[tokio::test]
async fn a_cross_file_error_is_still_reported() {
    let dir = tempfile::tempdir().unwrap();
    let tool = "name: dup\ndescription: d\nparameters: {type: object}\nbash: echo hi\n";
    write(
        dir.path(),
        "config.yaml",
        &format!(
            "cleanup_remote_session_days: 30\nscript_tools:\n{}",
            tool.lines()
                .enumerate()
                .map(|(index, line)| {
                    let prefix = if index == 0 { "  - " } else { "    " };
                    format!("{prefix}{line}\n")
                })
                .collect::<String>()
        ),
    );
    write(dir.path(), "packages/p/script_tools/dup.yaml", tool);

    let report = check(dir.path()).await;

    let codes: Vec<Code> = report.findings.iter().map(|finding| finding.code).collect();
    assert_eq!(
        codes,
        vec![Code::UnknownKey, Code::ParseError],
        "{:#?}",
        report.findings
    );
    assert!(
        report.findings[1].message.contains("dup"),
        "{:#?}",
        report.findings[1]
    );
}
