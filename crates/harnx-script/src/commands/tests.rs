use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::{BashKitBackend, ScriptBackend, ScriptLimits, ScriptRun, ScriptStatus};

#[derive(Default)]
struct FakeExecutor {
    calls: Mutex<Vec<(String, Value)>>,
}

#[async_trait]
impl InnerExecutor for FakeExecutor {
    async fn call(&self, name: &str, args: Value, _cancel: CancellationToken) -> CallOutcome {
        self.calls.lock().unwrap().push((name.into(), args.clone()));
        match name {
            "fs_read" => CallOutcome::Ok(
                json!({"content":[{"type":"text","text":"fallback text"}],"structuredContent":{"path":args["path"],"count":3}}),
            ),
            "text_tool" => CallOutcome::Ok(
                json!({"content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}),
            ),
            "mapped" => CallOutcome::Ok(
                json!({"content":[{"type":"text","text":"fallback"}],"structuredContent":{"text":"body","notes":["careful"],"code":3}}),
            ),
            "mapped_plain" => {
                CallOutcome::Ok(json!({"content":[{"type":"text","text":"plain text"}]}))
            }
            "mapped_broken" => CallOutcome::ToolError {
                message: "mapped failure".into(),
                partial_result: None,
            },
            "mapped_domain_error" => CallOutcome::Ok(
                json!({"content":[{"type":"text","text":"domain failure"}],"isError":true}),
            ),
            "approval" => CallOutcome::ApprovalRequired,
            "broken" => CallOutcome::ToolError {
                message: "tool failed".into(),
                partial_result: Some(json!({"partial":42})),
            },
            "domain_error" => CallOutcome::Ok(
                json!({"content":[{"type":"text","text":"domain failure"}],"isError":true}),
            ),
            "blocked" => CallOutcome::Blocked {
                reason: "policy block".into(),
            },
            "rejected" => CallOutcome::Rejected {
                reason: "call budget exceeded".into(),
                kind: RejectionKind::Other,
            },
            "unknown" => CallOutcome::Rejected {
                reason: "tool disappeared".into(),
                kind: RejectionKind::UnknownCommand,
            },
            "fatal" => CallOutcome::Fatal {
                message: "fatal execution failure".into(),
            },
            _ => panic!("unexpected dispatch {name}"),
        }
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        output: None,
        name: name.into(),
        description: format!("Run {name}. More detail."),
        schema: json!({"type":"object","properties":{}}),
    }
}

fn specs() -> Vec<ToolSpec> {
    let mut specs: Vec<_> = [
        "text_tool",
        "approval",
        "broken",
        "blocked",
        "rejected",
        "unknown",
        "fatal",
        "domain_error",
    ]
    .into_iter()
    .map(spec)
    .collect();
    for name in [
        "mapped",
        "mapped_plain",
        "mapped_broken",
        "mapped_domain_error",
    ] {
        let mut tool = spec(name);
        tool.output = Some(OutputMapping {
            stdout: Some("text".into()),
            stderr: Some("notes".into()),
            exit_code: Some("code".into()),
            error_exit_code: Some(2),
            ..Default::default()
        });
        specs.push(tool);
    }
    specs.push(ToolSpec {
        output: None,
        name: "fs_read".into(),
        description: "Read a file. Return contents.".into(),
        schema: json!({"properties":{"path":{"type":"string"}},"required":["path"]}),
    });
    specs
}

async fn run(script: &str, executor: Arc<FakeExecutor>) -> ScriptRun {
    let commands = tool_commands(specs(), executor).unwrap();
    BashKitBackend
        .run(crate::ScriptRequest {
            script,
            python_functions: None,
            limits: ScriptLimits {
                timeout_secs: 5,
                ..Default::default()
            },
            commands,
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        })
        .await
}

#[tokio::test]
async fn tool_pipeline_and_explicit_stdin_preserve_parsed_arguments() {
    for (script, expected) in [
        ("fs_read --path a | jq -r '.path'", "a\n"),
        (
            "printf '{\"path\":\"base\"}' | fs_read --json - --path override | jq -r '.path'",
            "override\n",
        ),
        (
            "printf invalid-json | fs_read --path a | jq -r '.path'",
            "a\n",
        ),
        ("fs_read --path a -o text", "fallback text\n"),
        ("text_tool -o json | jq -r '.content[0].text'", "one\n"),
        ("text_tool", "one\ntwo\n"),
    ] {
        let executor = Arc::new(FakeExecutor::default());
        let result = run(script, executor.clone()).await;
        assert_eq!(result.exit_code, 0, "{script}: {result:?}");
        assert_eq!(result.stdout, expected, "{script}");
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        if calls[0].0 == "fs_read" {
            assert!(calls[0].1.get("path").is_some());
            assert_eq!(
                calls[0].1.as_object().unwrap().len(),
                1,
                "rendering isn't a tool argument"
            );
        }
    }
}

#[tokio::test]
async fn ordinary_outcomes_have_distinct_exit_codes_and_errexit() {
    for (script, code, error) in [
        (
            "approval",
            126,
            "needs approval: approval; call it directly instead of from bash",
        ),
        ("broken", 1, "tool failed"),
        ("domain_error", 1, "domain failure"),
        ("blocked", 126, "policy block"),
        ("rejected", 126, "call budget exceeded"),
        ("unknown", 127, "tool disappeared"),
        ("fs_read --no-such-flag", 2, "unknown flag"),
        ("fs_read", 2, "missing required flags"),
        ("fs_read --path a -o xml", 2, "invalid -o value"),
    ] {
        let executor = Arc::new(FakeExecutor::default());
        let result = run(&format!("{script}; echo unreachable"), executor.clone()).await;
        assert_eq!(result.exit_code, code, "{script}: {result:?}");
        assert!(result.stderr.contains(error), "{script}: {result:?}");
        assert!(!result.stdout.contains("unreachable"));
        assert_eq!(executor.calls.lock().unwrap().len(), usize::from(code != 2));
        if script == "broken" {
            assert_eq!(result.stdout, "{\"partial\":42}\n");
        }
    }
    let result = run("set +e; approval; echo recovered", Arc::default()).await;
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "recovered\n");
}

#[tokio::test]
async fn stderr_names_why_a_command_may_not_run() {
    for (script, prefix) in [
        ("approval", "needs approval: "),
        ("blocked", "blocked: "),
        ("rejected", "rejected: "),
    ] {
        let result = run(script, Arc::default()).await;
        assert!(result.stderr.starts_with(prefix), "{script}: {result:?}");
    }
}

#[tokio::test]
async fn fatal_tool_aborts_foreground_and_background_despite_set_plus_e() {
    for script in [
        "set +e; fatal; echo unreachable",
        "set +e; fatal & wait; echo unreachable",
    ] {
        let result = run(script, Arc::default()).await;
        assert_eq!(result.status, ScriptStatus::Error);
        assert_ne!(result.exit_code, 0);
        assert!(
            result.stderr.contains("fatal execution failure"),
            "{result:?}"
        );
        assert!(!result.stdout.contains("unreachable"));
    }
}

#[tokio::test]
async fn missing_command_resolver_suggests_without_dispatching() {
    for script in ["fs_reed --path a", "help fs_reed"] {
        let executor = Arc::new(FakeExecutor::default());
        let result = run(script, executor.clone()).await;
        assert_eq!(result.exit_code, 127, "{result:?}");
        assert!(
            result
                .stderr
                .contains("no such command 'fs_reed'; did you mean 'fs_read'? run 'tools' to list"),
            "{result:?}"
        );
        assert!(executor.calls.lock().unwrap().is_empty());
    }
    let result = run("zzzzzzzzzz", Arc::default()).await;
    assert_eq!(result.exit_code, 127);
    assert!(!result.stderr.contains("did you mean"));
}

#[tokio::test]
async fn discovery_and_help_never_dispatch_tools() {
    for (script, expected) in [
        (
            "tools 'fs_*'",
            "fs_read --path <string> [--json <object>]  # Read a file.\n",
        ),
        ("tools 'nothing*'", ""),
        ("tools --help", super::discovery::TOOLS_HELP),
        ("help --help", super::discovery::HELP_HELP),
        ("help", super::discovery::HELP_HELP),
    ] {
        let executor = Arc::new(FakeExecutor::default());
        let result = run(script, executor.clone()).await;
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert_eq!(result.stdout, expected, "{script}");
        assert!(executor.calls.lock().unwrap().is_empty());
    }
    let executor = Arc::new(FakeExecutor::default());
    let direct = run("fs_read --help", executor.clone()).await;
    let help = run("help fs_read", executor.clone()).await;
    assert_eq!(direct.exit_code, 0);
    assert_eq!(direct.stdout, help.stdout);
    for section in ["NAME\n", "SYNOPSIS\n", "DESCRIPTION\n", "OPTIONS\n"] {
        assert!(direct.stdout.contains(section));
    }
    let multiple = run("help fs_read text_tool", executor.clone()).await;
    assert_eq!(multiple.exit_code, 0);
    assert_eq!(multiple.stdout.matches("NAME\n").count(), 2);
    // PR3 review N1: output-format flags alongside a standalone `--help`
    // still request help instead of a usage error.
    for script in ["fs_read --help -o text", "fs_read -o json --help"] {
        let result = run(script, executor.clone()).await;
        assert_eq!(result.exit_code, 0, "{script}: {result:?}");
        assert_eq!(result.stdout, direct.stdout, "{script}: {result:?}");
        assert!(result.stderr.is_empty(), "{script}: {result:?}");
    }
    let listing = run("tools", executor.clone()).await;
    assert_eq!(listing.stdout.lines().count(), specs().len());
    let invalid = run("tools '['", executor.clone()).await;
    assert_eq!(invalid.exit_code, 2);
    assert!(executor.calls.lock().unwrap().is_empty());
}

#[test]
fn duplicate_and_reserved_catalog_names_fail_before_execution() {
    assert!(tool_commands(
        vec![spec("a"), spec("a")],
        Arc::new(FakeExecutor::default())
    )
    .is_err());
    for name in ["tools", "help", "", "bad name", "/bin/echo", "line\nbreak"] {
        assert!(
            tool_commands(vec![spec(name)], Arc::new(FakeExecutor::default())).is_err(),
            "{name:?}"
        );
    }
}

#[tokio::test]
async fn mapped_tool_success_uses_its_mapping() {
    let result = run("mapped", Arc::new(FakeExecutor::default())).await;
    assert_eq!(result.stdout, "body");
    assert_eq!(result.stderr, "mapped: careful\n");
    assert_eq!(result.exit_code, 3);
}

#[tokio::test]
async fn mapped_tool_errors_exit_with_the_error_exit_code() {
    for script in ["mapped_broken", "mapped_domain_error"] {
        let result = run(script, Arc::new(FakeExecutor::default())).await;
        assert_eq!(result.exit_code, 2, "{script}: {result:?}");
    }
}

#[tokio::test]
async fn unmapped_tool_errors_still_exit_one() {
    let result = run("broken", Arc::new(FakeExecutor::default())).await;
    assert_eq!(result.exit_code, 1);
}

#[tokio::test]
async fn mapped_tool_with_a_plain_content_result_renders_its_text() {
    let result = run("mapped_plain", Arc::new(FakeExecutor::default())).await;
    assert_eq!(result.stdout, "plain text\n");
    assert_eq!(result.exit_code, 0);
}
