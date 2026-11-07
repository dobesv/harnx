use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{creator, reader};
use crate::{
    commands::{tool_commands, CallOutcome, InnerExecutor, ToolSpec},
    description::{bash_tool_description, examples, QUOTING},
    BashKitBackend, CancellationToken, ScriptBackend, ScriptLimits, ScriptRun, ScriptStatus,
};

#[derive(Default)]
struct Executor(Mutex<Vec<(String, Value)>>);

#[async_trait]
impl InnerExecutor for Executor {
    async fn call(&self, name: &str, args: Value, _: CancellationToken) -> CallOutcome {
        self.0.lock().unwrap().push((name.into(), args.clone()));
        if name.ends_with("fs_read") {
            assert_eq!(args, json!({"path":"src/lib.rs"}));
            CallOutcome::Ok(
                json!({"content":[{"type":"text", "text":"TODO first\nkeep\nTODO second\nTODO third\nTODO fourth\n"}]}),
            )
        } else {
            CallOutcome::Ok(json!({"structuredContent":args}))
        }
    }
}

async fn run(script: &str, tools: Vec<ToolSpec>, executor: Arc<Executor>) -> ScriptRun {
    BashKitBackend
        .run(crate::ScriptRequest {
            script,
            limits: ScriptLimits::default(),
            commands: tool_commands(tools, executor).unwrap(),
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        })
        .await
}

#[tokio::test]
async fn generated_pipeline_executes_actual_catalog_command() {
    let tools = vec![reader()];
    let description = bash_tool_description(&tools, 8, 1000);
    let script = description
        .split("Worked pipeline:")
        .nth(1)
        .unwrap()
        .split("```bash\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    let executor = Arc::new(Executor::default());
    let result = run(script, tools, executor.clone()).await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "1:TODO first\n3:TODO second\n4:TODO third\n");
    assert_eq!(executor.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn documented_quoting_idioms_execute() {
    let literal = QUOTING
        .split("```bash\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    let script = format!(
        r#"{literal}
printf '%s\n' "$text"
printf '%s\n' "$text" >/tmp/result.txt
printf '%s\n' "$(< /tmp/result.txt)"
printf '%q\n' "$text" >/tmp/quoted.txt
eval "copy=$(< /tmp/quoted.txt)"
printf '%s\n' "$copy"
printf 'one\ntwo\n' | xargs -n1 printf '[%s]\n'
printf 'PNG' >/tmp/x.png
printf '%s\n' "$(base64 -w0 /tmp/x.png)"
"#
    );
    let result = run(&script, vec![], Arc::new(Executor::default())).await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "literal $variables and \"quotes\"\nliteral $variables and \"quotes\"\nliteral $variables and \"quotes\"\n[one]\n[two]\nUE5H\n");
}

#[tokio::test]
async fn generated_attachment_idiom_passes_base64_and_handles_quoted_names() {
    let mut tool = creator(true);
    tool.name = "media'attachment_create".into();
    let script = format!(
        "printf 'PNG' >/tmp/x.png\n{}",
        examples::attachment_idiom(&[&tool]).unwrap()
    );
    let executor = Arc::new(Executor::default());
    let result = run(&script, vec![tool.clone()], executor.clone()).await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(
        *executor.0.lock().unwrap(),
        vec![(
            tool.name,
            json!({
                "mime_type":"image/png", "encoding":"base64", "content":"UE5H"
            })
        )]
    );
}

#[tokio::test]
async fn documented_errexit_exceptions_and_pipeline_statuses_match_backend() {
    for script in [
        "if false; then echo wrong; fi; echo survived",
        "while false; do echo wrong; done; echo survived",
        "false && echo wrong; echo survived",
        "true || echo wrong; echo survived",
        "! true; echo survived",
        "false | true; echo survived",
        "set +e; false; echo survived",
    ] {
        let result = run(script, vec![], Arc::new(Executor::default())).await;
        assert_eq!(result.status, ScriptStatus::Ok, "{script}: {result:?}");
        assert_eq!(result.stdout, "survived\n", "{script}: {result:?}");
    }
    for script in [
        "false; echo wrong",
        "true && false; echo wrong",
        "false || false; echo wrong",
        "true | false; echo wrong",
        "set -o pipefail; false | true; echo wrong",
    ] {
        let result = run(script, vec![], Arc::new(Executor::default())).await;
        assert_eq!(result.status, ScriptStatus::Error, "{script}: {result:?}");
        assert_eq!(result.exit_code, 1, "{script}: {result:?}");
        assert!(result.stdout.is_empty(), "{script}: {result:?}");
    }
}

fn spawn_tool() -> ToolSpec {
    ToolSpec {
        name: "bash_spawn".into(),
        description: "Spawn a background command, optionally waiting for completion.".into(),
        // Optional Schemars objects can be nullable references, not inline objects.
        schema: json!({
            "type": "object", "required": ["command"],
            "properties": {
                "command": {"type": "string"},
                "wait": {"anyOf": [{"$ref": "#/$defs/WaitOptions"}, {"type": "null"}]}
            },
            "$defs": {"WaitOptions": {"type": "object", "properties": {
                "timeout_secs": {"type": ["integer", "null"]},
                "head_lines": {"type": ["integer", "null"]},
                "tail_lines": {"type": ["integer", "null"]},
                "max_output_bytes": {"type": ["integer", "null"]},
                "grep": {"type": ["string", "null"]}
            }}}
        }),
    }
}

#[test]
fn generated_guide_advertises_nested_wait_as_optional_json() {
    let tool = spawn_tool();
    let description = bash_tool_description(std::slice::from_ref(&tool), 8, 1000);
    assert!(description.contains(
        "- bash_spawn --command <string> [--wait <json>] [--json <object>]  # Spawn a background command, optionally waiting for completion."
    ), "{description}");
    assert!(description.contains("objects/complex values take quoted JSON"));
    assert!(!description.contains("--wait-secs"));
    assert!(!description.contains("--timeout-secs"));
    let help = crate::help::help(&tool.name, &tool.schema, &tool.description);
    assert!(help.contains("--wait <json>"), "{help}");
    assert!(!help.contains("--head-lines"), "{help}");
}

#[tokio::test]
async fn documented_json_flags_preserve_nested_wait_through_dispatch() {
    let tool = spawn_tool();
    let executor = Arc::new(Executor::default());
    let waits = [
        json!({}),
        json!({"timeout_secs": 0, "head_lines": 0, "tail_lines": 0,
            "max_output_bytes": 0, "grep": ""}),
        json!({"timeout_secs": 600, "head_lines": 2, "tail_lines": 3,
            "max_output_bytes": 1024, "grep": "error|warn"}),
    ];
    for wait in waits {
        for script in [
            format!("bash_spawn --command 'echo ready' --wait '{wait}'"),
            format!("bash_spawn --json '{{\"command\":\"echo ready\",\"wait\":{wait}}}'"),
        ] {
            let result = run(&script, vec![tool.clone()], executor.clone()).await;
            assert_eq!(result.status, ScriptStatus::Ok, "{script}: {result:?}");
            assert_eq!(
                executor.0.lock().unwrap().last().cloned(),
                Some((
                    tool.name.clone(),
                    json!({"command": "echo ready", "wait": wait})
                )),
                "{script}"
            );
        }
    }
    let result = run(
        "bash_spawn --command 'echo ready'",
        vec![tool.clone()],
        executor.clone(),
    )
    .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(
        executor.0.lock().unwrap().last().cloned(),
        Some((tool.name, json!({"command": "echo ready"})))
    );
    assert_eq!(executor.0.lock().unwrap().len(), 7);
}
