//! What the loaders accept without a word, reported as findings.

use std::path::Path;

use harnx_runtime::config_check::{Code, Severity};

use super::{check, check_holding, with_code, write, EnvVarGuard};

#[tokio::test]
async fn bad_package_client_reports_whole_package_drop() {
    let report = check_holding(&[
        ("packages/p/clients/good.yaml", "type: openai\n"),
        ("packages/p/clients/bad.yaml", "type: openai\napi_key: [\n"),
    ])
    .await;

    assert_eq!(with_code(&report, Code::ParseError).len(), 1, "{report:#?}");
    let drops = with_code(&report, Code::SilentDrop);
    assert_eq!(drops.len(), 1, "{report:#?}");
    assert_eq!(drops[0].severity, Severity::Error);
    assert!(
        drops[0]
            .message
            .contains("all clients in package 'p' are dropped at runtime"),
        "{}",
        drops[0].message
    );
    assert!(
        drops[0].message.contains("bad.yaml"),
        "{}",
        drops[0].message
    );
}

#[tokio::test]
async fn failed_client_patch_is_error() {
    let report = check_holding(&[
        ("packages/p/clients/c.yaml", "type: openai\n"),
        (
            "packages/p.patch.yaml",
            "clients:\n  - \"this is not jq (\"\n",
        ),
    ])
    .await;

    let drops = with_code(&report, Code::SilentDrop);
    assert_eq!(drops.len(), 1, "{report:#?}");
    assert_eq!(drops[0].severity, Severity::Error);
    assert_eq!(
        drops[0].file.as_deref(),
        Some(Path::new("packages/p.patch.yaml"))
    );
    assert!(
        drops[0].message.contains("client 'p/c'"),
        "{}",
        drops[0].message
    );
}

#[tokio::test]
async fn working_client_patch_is_clean() {
    let report = check_holding(&[
        ("packages/p/clients/c.yaml", "type: openai\n"),
        (
            "packages/p.patch.yaml",
            "clients:\n  - \".api_base = \\\"http://x\\\"\"\n",
        ),
    ])
    .await;

    assert!(
        with_code(&report, Code::SilentDrop).is_empty(),
        "{report:#?}"
    );
}

#[tokio::test]
async fn models_override_version_mismatch_warns() {
    let report = check_holding(&[("models-override.yaml", "version: 0.0.1\nlist: []\n")]).await;

    let found = with_code(&report, Code::ModelsOverrideVersion);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert!(found[0].message.contains("0.0.1"), "{}", found[0].message);
}

#[tokio::test]
async fn env_invalid_line_warns_with_line() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "config.yaml", "stream: true\n");
    write(dir.path(), ".env", "A=1\nnot a pair\n# note\n\n=x\n");
    let _env_file = EnvVarGuard::set("HARNX_ENV_FILE", &dir.path().join(".env"));

    let report = check(dir.path()).await;

    let lines: Vec<_> = with_code(&report, Code::EnvLineInvalid)
        .iter()
        .map(|f| (f.severity, f.line, f.file.clone()))
        .collect();
    let file = Some(Path::new(".env").to_path_buf());
    assert_eq!(
        lines,
        vec![
            (Severity::Warning, Some(2), file.clone()),
            (Severity::Warning, Some(5), file)
        ],
        "{report:#?}"
    );
}

#[tokio::test]
async fn front_matter_after_blank_line_warns() {
    let report = check_holding(&[("agents/a.md", "\n---\nmodel: x\n---\nhi\n")]).await;

    let found = with_code(&report, Code::FrontMatterNotAtStart);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert_eq!(found[0].line, Some(2));
    assert_eq!(found[0].file.as_deref(), Some(Path::new("agents/a.md")));
}

#[tokio::test]
async fn unknown_compaction_agent_is_error() {
    let report =
        check_holding(&[("agents/a.md", "---\ncompaction_agent: missing\n---\nhi\n")]).await;

    let found = with_code(&report, Code::AgentNotFound);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Error);
    assert!(
        found[0].message.contains("compaction_agent"),
        "{}",
        found[0].message
    );
    assert!(
        found[0].message.contains("'missing'"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn unknown_global_title_agent_is_error() {
    let report = check_holding(&[("config.yaml", "title_agent: missing\n")]).await;

    let found = with_code(&report, Code::AgentNotFound);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].file.as_deref(), Some(Path::new("config.yaml")));
}

#[tokio::test]
async fn package_relative_title_agent_resolves() {
    let report = check_holding(&[
        ("packages/p/agents/a.md", "---\ntitle_agent: t\n---\nhi\n"),
        ("packages/p/agents/t.md", "titles\n"),
    ])
    .await;

    assert!(
        with_code(&report, Code::AgentNotFound).is_empty(),
        "{report:#?}"
    );
}

#[tokio::test]
async fn package_agent_title_agent_falls_back_to_top_level() {
    let report = check_holding(&[
        (
            "packages/p/agents/a.md",
            "---\ntitle_agent: titler\n---\nhi\n",
        ),
        ("agents/titler.md", "titles\n"),
    ])
    .await;

    assert!(
        with_code(&report, Code::AgentNotFound).is_empty(),
        "{report:#?}"
    );
}

#[tokio::test]
async fn package_agent_compaction_agent_does_not_fall_back() {
    let report = check_holding(&[
        (
            "packages/p/agents/a.md",
            "---\ncompaction_agent: sum\n---\nhi\n",
        ),
        ("agents/sum.md", "sums\n"),
    ])
    .await;

    assert_eq!(
        with_code(&report, Code::AgentNotFound).len(),
        1,
        "{report:#?}"
    );
}

#[tokio::test]
async fn remote_agent_reference_is_skipped() {
    let report = check_holding(&[(
        "agents/a.md",
        "---\ntitle_agent: titler@cluster\ncompaction_agent: sum@cluster\n---\nhi\n",
    )])
    .await;

    assert!(
        with_code(&report, Code::AgentNotFound).is_empty(),
        "{report:#?}"
    );
}
