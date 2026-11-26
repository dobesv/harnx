//! `tools` in the `bash` tool's Python, against fake tools.

use super::*;

struct PyTools;

#[async_trait::async_trait]
impl ToolProvider for PyTools {
    fn name(&self) -> &str {
        "py-tools"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        py_tool_declarations().iter().any(|d| d.name == tool_name)
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        match name {
            "echo" => Ok(ToolProviderOutput::new(json!({
                "content": [{"type": "text", "text": "echoed"}],
                "structuredContent": {"text": args["text"], "exit_code": 0},
            }))),
            "cut" => Ok(ToolProviderOutput::new(json!({
                "content": [{"type": "text", "text": "partial"}],
                "structuredContent": {
                    "text": "partial",
                    "notes": "output cut off at 10 bytes",
                    "exit_code": 3,
                },
            }))),
            "boom" => Err(ToolError::Recoverable(error_with_partial_result(
                anyhow!("it broke"),
                json!({"half": "done"}),
            ))),
            "fatal" => Err(ToolError::Fatal(anyhow!("worker lost"))),
            "pause" => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(ToolProviderOutput::new(json!({"content": []})))
            }
            other => Ok(ToolProviderOutput::new(json!({
                "content": [{"type": "text", "text": format!("{other}:{args}")}],
            }))),
        }
    }
}

fn py_declaration(
    name: &str,
    properties: Value,
    required: Value,
) -> harnx_core::tool::ToolDeclaration {
    let mut declaration = inner_declarations().remove(0);
    declaration.name = name.to_string();
    declaration.parameters =
        serde_json::from_value(json!({"properties": properties, "required": required})).unwrap();
    declaration
}

fn py_tool_declarations() -> Vec<harnx_core::tool::ToolDeclaration> {
    let text = json!({"text": {"type": "string"}});
    vec![
        py_declaration("echo", text, json!(["text"])),
        py_declaration("plain", json!({"x": {"type": "string"}}), json!([])),
        py_declaration("cut", json!({}), json!([])),
        py_declaration("boom", json!({}), json!([])),
        py_declaration("fatal", json!({}), json!([])),
        py_declaration("pause", json!({}), json!([])),
        py_declaration("my-tool", json!({"x": {"type": "string"}}), json!([])),
        py_declaration("kw", json!({"from": {"type": "integer"}}), json!([])),
    ]
}

fn py_harness() -> Harness {
    build_harness_with(
        Arc::new(PyTools),
        Vec::new(),
        None,
        true,
        (py_tool_declarations(), Default::default()),
    )
}

fn heredoc(python: &str) -> String {
    format!("python3 - <<'EOF'\n{python}\nEOF")
}

async fn run_python(bash: &BashProvider, python: &str) -> Result<ToolProviderOutput, ToolError> {
    run(bash, &heredoc(python), &create_abort_signal()).await
}

fn failed_partial(result: Result<ToolProviderOutput, ToolError>) -> Value {
    let error = recoverable(result, "a failed script");
    partial_result_of(&error)
        .expect("a failed run carries its output")
        .clone()
}

fn trace_names(payload: &Value) -> Vec<String> {
    payload["trace"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_calls_a_tool_and_gets_its_fields() {
    let (bash, _slot, _ctx) = py_harness();
    let output = ok(run_python(
        &bash,
        "r = tools.echo(text='hi')\nprint(r['text'], r['exit_code'])",
    )
    .await);
    assert_eq!(output.value["stdout"], "hi 0\n", "{}", output.value);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_gets_text_from_a_tool_without_structured_content() {
    let (bash, _slot, _ctx) = py_harness();
    let output = ok(run_python(&bash, "print(repr(tools.plain(x='a').strip()))").await);
    assert_eq!(output.value["stdout"], "'plain:{\"x\":\"a\"}'\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_passes_required_arguments_positionally() {
    let (bash, _slot, _ctx) = py_harness();
    let output = ok(run_python(&bash, "print(tools.echo('hi')['text'])").await);
    assert_eq!(output.value["stdout"], "hi\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_argument_is_a_type_error_before_the_call() {
    let (bash, _slot, _ctx) = py_harness();
    let python = "try:\n    tools.echo(bogus=1)\nexcept TypeError as e:\n    print('caught', e)";
    let output = ok(run_python(&bash, python).await);
    assert_eq!(
        output.value["stdout"],
        "caught echo() got an unexpected keyword argument 'bogus'\n"
    );
    assert!(output.value["trace"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_error_is_a_runtime_error_the_script_can_catch() {
    let (bash, _slot, _ctx) = py_harness();
    let python = "try:\n    tools.boom()\nexcept RuntimeError as e:\n    print(e)";
    let output = ok(run_python(&bash, python).await);
    assert_eq!(
        output.value["stdout"],
        "boom: it broke; partial result: {\"half\":\"done\"}\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_call_says_why() {
    let (bash, slot, ctx) = py_harness();
    let mut blocking = (*ctx).clone();
    blocking.middleware_fn = Some(Arc::new(|prep, _, _| {
        Box::pin(async move {
            Ok(harnx_engine::tool::ToolMiddlewareOutcome::Completed(
                harnx_engine::tool::MaterializedToolResult::new(
                    harnx_core::tool::ToolResult::new(
                        prep.call.clone(),
                        json!({"error": "denied by policy"}),
                    ),
                    harnx_engine::tool::InnerToolCallStatus::Blocked,
                ),
            ))
        })
    }));
    let blocking = Arc::new(blocking);
    slot.set(&blocking);
    let python = "try:\n    tools.plain()\nexcept RuntimeError as e:\n    print(e)";
    let output = ok(run_python(&bash, python).await);
    assert_eq!(output.value["stdout"], "plain: blocked: denied by policy\n");
}

#[test]
fn a_call_that_needs_approval_names_the_direct_call() {
    let outcome = crate::bash_inner::InnerOutcome::ApprovalRequired {
        tool: "fs_write".into(),
        reason: None,
    };
    match super::super::python_tools::map_outcome("fs_write", outcome) {
        super::super::python_tools::Mapped::Raise(message) => assert_eq!(
            message,
            "fs_write: needs approval; call fs_write directly, outside the script"
        ),
        _ => panic!("expected a raised error"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fatal_outcome_ends_the_run_despite_except_exception() {
    let (bash, _slot, _ctx) = py_harness();
    let python =
        "try:\n    tools.fatal()\nexcept Exception:\n    print('swallowed')\nprint('after')";
    let partial = failed_partial(run_python(&bash, python).await);
    assert_eq!(partial["status"], "error", "{partial}");
    assert_eq!(partial["error"], "worker lost", "{partial}");
    assert_eq!(partial["exit_code"], 1, "{partial}");
    assert!(
        partial["stderr"].as_str().unwrap().contains("worker lost"),
        "{partial}"
    );
    assert!(!partial["stdout"].as_str().unwrap().contains("swallowed"));
    assert!(!partial["stdout"].as_str().unwrap().contains("after"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_and_bash_calls_share_the_call_budget() {
    let (bash, _slot, _ctx) = py_harness();
    let script = format!(
        "for i in $(seq 1 500); do plain --x a > /dev/null; done\n{}",
        heredoc(
            "for i in range(499):\n    tools.plain(x='a')\n\
             print('last ok', tools.plain(x='a') is not None)\n\
             try:\n    tools.plain(x='a')\nexcept RuntimeError as e:\n    print(e)"
        )
    );
    let output = ok(run(&bash, &script, &create_abort_signal()).await);
    let stdout = output.value["stdout"].as_str().unwrap();
    assert!(stdout.contains("last ok True"), "{stdout}");
    assert!(
        stdout.contains("plain: bash script exceeded its limit of 1000 inner calls"),
        "{stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_calls_appear_in_the_trace() {
    let (bash, _slot, _ctx) = py_harness();
    let script = format!(
        "plain --x a\n{}\necho --text b",
        heredoc("tools.echo(text='p')\ntools.boom() if False else None")
    );
    let output = ok(run(&bash, &script, &create_abort_signal()).await);
    assert_eq!(trace_names(&output.value), ["plain", "echo", "echo"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_with_a_dash_in_its_name_is_reached_with_getattr() {
    let (bash, _slot, _ctx) = py_harness();
    let output = ok(run_python(&bash, "print(getattr(tools, 'my-tool')(x='a').strip())").await);
    assert_eq!(output.value["stdout"], "my-tool:{\"x\":\"a\"}\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_property_named_like_a_keyword_is_passed_with_a_dict() {
    let (bash, _slot, _ctx) = py_harness();
    let output = ok(run_python(&bash, "print(tools.kw(**{'from': 1}).strip())").await);
    assert_eq!(output.value["stdout"], "kw:{\"from\":1}\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_longer_than_the_run_times_out() {
    let (bash, _slot, _ctx) = py_harness();
    let started = std::time::Instant::now();
    let result = bash
        .call_tool(
            BASH_TOOL_NAME,
            json!({"script": heredoc("tools.pause()"), "timeout_secs": 1}),
            &create_abort_signal(),
        )
        .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(failed_partial(result)["status"], "timed_out");
}

/// The claims of the description's Python paragraph, through the real binder:
/// keyword and positional calls, a bad keyword as `TypeError`, a tool failure
/// as `RuntimeError`, and a non-zero `exit_code` and `notes` read from the dict.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_descriptions_python_claims_hold_in_one_script() {
    let (bash, _slot, _ctx) = py_harness();
    let python = "\
a = tools.echo(text='kw')\n\
b = tools.echo('pos')\n\
print(a['text'], b['text'])\n\
try:\n    tools.echo(text='x', bogus=1)\nexcept TypeError as e:\n    print('TypeError', e)\n\
try:\n    tools.boom()\nexcept RuntimeError as e:\n    print('RuntimeError', str(e)[:10])\n\
c = tools.cut()\n\
print(c['exit_code'], c['notes'])";
    let output = ok(run_python(&bash, python).await);
    assert_eq!(
        output.value["stdout"],
        "kw pos\n\
         TypeError echo() got an unexpected keyword argument 'bogus'\n\
         RuntimeError boom: it b\n\
         3 output cut off at 10 bytes\n",
        "{}",
        output.value
    );
}
