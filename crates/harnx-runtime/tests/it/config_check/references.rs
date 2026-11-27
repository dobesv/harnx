//! References from one file to something defined elsewhere, or outside the
//! directory: access rule agents, environment variables, tool server
//! commands.

use std::path::Path;

use harnx_runtime::config_check::{Code, Severity};

use super::{check_holding, with_code, EnvVarGuard};

#[tokio::test]
async fn access_pattern_matching_nothing_warns() {
    let report = check_holding(&[
        ("packages/coding/agents/coder.md", "codes\n"),
        (
            "access.yaml",
            "rules:\n  - agents: [ghost, \"coding/*\"]\n    users: [bob]\n",
        ),
    ])
    .await;

    let found = with_code(&report, Code::AccessAgentUnmatched);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert_eq!(found[0].file.as_deref(), Some(Path::new("access.yaml")));
    assert!(
        found[0].message.contains("rule 0") && found[0].message.contains("'ghost'"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn remote_agent_counts_for_access() {
    let report = check_holding(&[
        ("config.yaml", "stream: true\n"),
        (
            "nats_servers/shared.yaml",
            "url: nats://localhost:4222\nagents:\n  - name: sisyphus\n",
        ),
        (
            "access.yaml",
            "rules:\n  - agents: [\"sisyphus@shared\"]\n    users: [bob]\n",
        ),
    ])
    .await;

    assert!(report.findings.is_empty(), "{report:#?}");
}

#[tokio::test]
async fn unset_nats_env_var_warns() {
    let report = check_holding(&[(
        "nats_servers/x.yaml",
        "url: ${HARNX_TEST_UNSET_VAR}\ntoken: ${HARNX_TEST_UNSET_TOKEN:-fallback}\n",
    )])
    .await;

    let found = with_code(&report, Code::EnvVarUnset);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert_eq!(
        found[0].file.as_deref(),
        Some(Path::new("nats_servers/x.yaml"))
    );
    assert_eq!(found[0].key_path.as_deref(), Some("url"));
    assert!(
        found[0].message.contains("HARNX_TEST_UNSET_VAR"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn set_nats_env_var_is_clean() {
    let _var = EnvVarGuard::set("HARNX_TEST_SET_VAR", Path::new("nats://localhost:4222"));
    let report = check_holding(&[
        ("config.yaml", "stream: true\n"),
        ("nats_servers/x.yaml", "url: ${HARNX_TEST_SET_VAR}\n"),
    ])
    .await;

    assert!(report.findings.is_empty(), "{report:#?}");
}

#[tokio::test]
async fn missing_tool_server_command_warns() {
    let report = check_holding(&[
        (
            "tool_servers/t.yaml",
            "command: definitely-not-installed-xyz\n",
        ),
        (
            "tool_servers/off.yaml",
            "command: definitely-not-installed-xyz\nenabled: false\n",
        ),
    ])
    .await;

    let found = with_code(&report, Code::CommandNotFound);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert_eq!(
        found[0].file.as_deref(),
        Some(Path::new("tool_servers/t.yaml"))
    );
    assert!(
        found[0].message.contains("definitely-not-installed-xyz"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn missing_package_tool_server_command_names_its_file() {
    let report = check_holding(&[(
        "packages/p/tool_servers/t.yaml",
        "command: definitely-not-installed-xyz\n",
    )])
    .await;

    let found = with_code(&report, Code::CommandNotFound);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(
        found[0].file.as_deref(),
        Some(Path::new("packages/p/tool_servers/t.yaml"))
    );
}
