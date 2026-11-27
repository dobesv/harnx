//! Every agent loaded and rendered, and the models agents and `config.yaml`
//! name.

use std::path::Path;

use harnx_runtime::config_check::{Code, Severity};

use super::{check_holding, with_code};

#[tokio::test]
async fn broken_agent_template_is_error() {
    let report = check_holding(&[("agents/a.md", "{{ undefined_var }}\n")]).await;

    let found = with_code(&report, Code::AgentLoad);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Error);
    assert_eq!(found[0].file.as_deref(), Some(Path::new("agents/a.md")));
    assert!(
        found[0].message.contains("agent 'a'"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn missing_variable_path_is_error() {
    let report = check_holding(&[(
        "agents/a.md",
        "---\nvariables:\n  - name: x\n    description: x\n    path: missing.md\n---\nhi {{ x }}\n",
    )])
    .await;

    let found = with_code(&report, Code::AgentLoad);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].file.as_deref(), Some(Path::new("agents/a.md")));
    assert!(
        found[0].message.contains("missing.md"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn agent_with_parse_error_gets_no_load_finding() {
    let report = check_holding(&[("agents/a.md", "---\nmodel: [\n---\nhi\n")]).await;

    assert_eq!(with_code(&report, Code::ParseError).len(), 1, "{report:#?}");
    assert!(
        with_code(&report, Code::AgentLoad).is_empty(),
        "{report:#?}"
    );
}

#[tokio::test]
async fn unknown_client_in_model_is_error() {
    let report = check_holding(&[("agents/a.md", "---\nmodel: nope:gpt-x\n---\nhi\n")]).await;

    let found = with_code(&report, Code::ModelUnresolved);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Error);
    assert_eq!(found[0].file.as_deref(), Some(Path::new("agents/a.md")));
    assert_eq!(found[0].key_path.as_deref(), Some("model"));
    assert!(
        found[0].message.contains("nope:gpt-x"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn unknown_fallback_model_is_error() {
    let report = check_holding(&[
        ("clients/openai.yaml", "type: openai\n"),
        (
            "agents/a.md",
            "---\nmodel: openai:gpt-5\nmodel_fallbacks: [nope:gpt-x]\n---\nhi\n",
        ),
    ])
    .await;

    let found = with_code(&report, Code::ModelUnresolved);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].key_path.as_deref(), Some("model_fallbacks"));
}

#[tokio::test]
async fn unknown_model_on_known_client_warns() {
    let report = check_holding(&[
        ("clients/openai.yaml", "type: openai\n"),
        ("agents/a.md", "---\nmodel: openai:not-a-model\n---\nhi\n"),
    ])
    .await;

    assert!(
        with_code(&report, Code::ModelUnresolved).is_empty(),
        "{report:#?}"
    );
    let found = with_code(&report, Code::ModelNotInCatalog);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert!(
        found[0].message.contains("not-a-model"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn package_agent_model_uses_package_client() {
    let report = check_holding(&[
        ("packages/p/clients/c.yaml", "type: openai\n"),
        ("packages/p/agents/a.md", "---\nmodel: c:gpt-5\n---\nhi\n"),
    ])
    .await;

    assert!(report.findings.is_empty(), "{report:#?}");
}

#[tokio::test]
async fn config_model_is_checked() {
    let report = check_holding(&[("config.yaml", "model: nope:gpt-x\n")]).await;

    let found = with_code(&report, Code::ModelUnresolved);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].file.as_deref(), Some(Path::new("config.yaml")));
}

#[tokio::test]
async fn declared_variable_without_default_renders() {
    let report = check_holding(&[
        ("config.yaml", "stream: true\n"),
        (
            "agents/a.md",
            "---\nvariables:\n  - name: topic\n    description: what to write about\n---\nWrite about {{ topic }}.\n",
        ),
    ])
    .await;

    assert!(report.findings.is_empty(), "{report:#?}");
}

#[tokio::test]
async fn undeclared_variable_is_still_error() {
    let report = check_holding(&[
        ("config.yaml", "stream: true\n"),
        (
            "agents/a.md",
            "---\nvariables:\n  - name: topic\n    description: what to write about\n---\nWrite about {{ x }}.\n",
        ),
    ])
    .await;

    let found = with_code(&report, Code::AgentLoad);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].file.as_deref(), Some(Path::new("agents/a.md")));
}
