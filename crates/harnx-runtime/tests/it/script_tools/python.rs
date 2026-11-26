//! Python script-tool bodies (T3): Monty runs through the same backend,
//! inner run and budgets as Bash; `tool_input` and `tools` are Python host
//! functions over the same `InnerCallRun` the Bash CLI commands use.

use super::*;

/// Plan success criterion: one Bash and one Python script tool wrap the
/// same existing tool, both selected via `use_tools`, and the wrapped tool
/// stays separately callable.
#[tokio::test(flavor = "multi_thread")]
async fn python_and_bash_wrappers_share_the_wrapped_tool_with_the_model() -> Result<()> {
    let config = test_config(
        &[
            script_tool(
                "wrap_bash",
                "probe_run --value from-bash",
                json!(["probe_run"]),
            ),
            python_tool(
                "wrap_python",
                r#"result = tools.probe_run(value="from-python")
print(result["value"])"#,
                json!(["probe_run"]),
            ),
        ],
        "---\nuse_tools: [wrap_bash, wrap_python, probe_run]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let direct = eval_one(&fixture.ctx, "probe_run", json!({"value": "direct"})).await;
    assert!(!is_error(&direct), "the original stays callable: {direct}");
    let via_bash = eval_one(&fixture.ctx, "wrap_bash", json!({"number": 1})).await;
    assert!(!is_error(&via_bash), "{via_bash}");
    let via_python = eval_one(&fixture.ctx, "wrap_python", json!({"number": 2})).await;
    assert!(!is_error(&via_python), "{via_python}");
    assert!(
        display_text(&via_python).contains("from-python"),
        "the inner call output must reach the python body: {via_python}"
    );

    let calls = probe.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].1["value"], "direct");
    assert_eq!(calls[1].1["value"], "from-bash");
    assert_eq!(calls[2].1["value"], "from-python");
    Ok(())
}

/// `tool_input()` returns the validated arguments as a dict (R5 for python).
#[tokio::test(flavor = "multi_thread")]
async fn python_tool_input_returns_the_validated_arguments() -> Result<()> {
    let config = test_config(
        &[python_tool(
            "echo_input",
            r#"args = tool_input()
print(args["number"])"#,
            json!([]),
        )],
        "---\nuse_tools: [echo_input]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "echo_input", json!({"number": 2813})).await;
    assert!(!is_error(&output), "{output}");
    assert_eq!(display_text(&output).trim(), "2813");
    assert!(probe.calls().is_empty(), "no inner dispatch happens");
    Ok(())
}

/// `tools` uses the shared binding: plain and `**` keyword forms both
/// work and the result converts back to Python values.
#[tokio::test(flavor = "multi_thread")]
async fn python_call_tool_binds_arguments_and_returns_values() -> Result<()> {
    let config = test_config(
        &[python_tool(
            "call_wrap",
            r#"r1 = tools.probe_run(value="positional")
r2 = tools.probe_run(**{"value": "keyword"})
print(r1["value"])
print(r2["value"])"#,
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [call_wrap]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "call_wrap", json!({"number": 1})).await;
    assert!(!is_error(&output), "{output}");
    let text = display_text(&output);
    assert!(text.contains("positional"), "{output}");
    assert!(text.contains("keyword"), "{output}");
    assert_eq!(probe.call_count(), 2);
    Ok(())
}

/// `call_tool` is not a function in a script tool's Python: a tool is an
/// attribute of `tools`, so the old spelling fails as Python's own `NameError`.
#[tokio::test(flavor = "multi_thread")]
async fn call_tool_is_gone() -> Result<()> {
    let config = test_config(
        &[python_tool(
            "old_wrap",
            r#"call_tool("probe_run", {})"#,
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [old_wrap]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "old_wrap", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    let error = output.to_string();
    assert!(
        error.contains("NameError") && error.contains("call_tool"),
        "{error}"
    );
    assert!(probe.calls().is_empty());
    Ok(())
}

/// Non-allowlisted tools, `bash`, control tools and other script tools are
/// not attributes of `tools`, so each raises a catchable `AttributeError`.
#[tokio::test(flavor = "multi_thread")]
async fn python_refusals_raise_catchable_named_exceptions() -> Result<()> {
    let config = test_config(
        &[
            python_tool(
                "refuse_wrap",
                r#"count = 0
for name in ["probe_aux", "bash", "ask_user", "ghost_tool", "helper_wrap"]:
    try:
        getattr(tools, name)()
    except AttributeError:
        count = count + 1
print(count)"#,
                json!(["probe_run"]),
            ),
            script_tool(
                "helper_wrap",
                "probe_run --value helper",
                json!(["probe_run"]),
            ),
        ],
        "---\nuse_tools: [refuse_wrap, helper_wrap]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "refuse_wrap", json!({"number": 1})).await;
    assert!(!is_error(&output), "every refusal is caught: {output}");
    assert_eq!(
        display_text(&output).trim(),
        "5",
        "all five excluded names must raise: {output}"
    );
    assert!(
        probe.calls().is_empty(),
        "no refused call may reach the provider: {:?}",
        probe.calls()
    );
    Ok(())
}

/// An inner tool error raises with the tool name and error text; uncaught,
/// it fails the run (decision 11).
#[tokio::test(flavor = "multi_thread")]
async fn python_inner_tool_error_raises_and_fails_the_run() -> Result<()> {
    let config = test_config(
        &[python_tool(
            "boom_wrap",
            r#"tools.probe_run(recoverable=True)"#,
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [boom_wrap]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe).await;

    let output = eval_one(&fixture.ctx, "boom_wrap", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    let error = output["error"].as_str().expect("error message");
    assert!(error.contains("probe_run"), "tool name expected: {error}");
    assert!(
        error.contains("recoverable probe failure"),
        "inner error text expected: {error}"
    );
    Ok(())
}

/// Middleware denials raise like inner errors (decision 11).
#[tokio::test(flavor = "multi_thread")]
async fn python_middleware_denial_raises() -> Result<()> {
    let deny = json!({"name": "deny-probe", "match": "^probe_run$", "bash": "deny 'not allowed'"});
    let config = test_config(
        &[python_tool(
            "deny_wrap",
            r#"tools.probe_run()"#,
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [deny_wrap]\n---\n",
        vec![deny],
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "deny_wrap", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    let error = output["error"].as_str().expect("error message");
    assert!(error.contains("probe_run"), "tool name expected: {error}");
    assert!(
        error.contains("not allowed"),
        "denial reason expected: {error}"
    );
    assert!(probe.calls().is_empty(), "the provider is never dispatched");
    Ok(())
}

/// An inner call that needs interaction is refused from a script body just
/// like from a Bash body: it raises instead of blocking (decision 11).
#[tokio::test(flavor = "multi_thread")]
async fn python_interaction_requirement_raises() -> Result<()> {
    let confirm = json!({"name": "confirm-probe", "match": "^probe_run$", "bash": "confirm --id gate 'sure?'"});
    let config = test_config(
        &[python_tool(
            "confirm_wrap",
            r#"tools.probe_run()"#,
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [confirm_wrap]\n---\n",
        vec![confirm],
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "confirm_wrap", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    let error = output["error"].as_str().expect("error message");
    assert!(error.contains("probe_run"), "tool name expected: {error}");
    assert!(
        error.contains("interaction required"),
        "the interaction refusal must be visible: {error}"
    );
    Ok(())
}

/// Any other uncaught Python exception becomes a normal error result.
#[tokio::test(flavor = "multi_thread")]
async fn python_uncaught_exception_is_an_error_result() -> Result<()> {
    let config = test_config(
        &[python_tool(
            "raise_wrap",
            "raise ValueError('kaboom')",
            json!([]),
        )],
        "---\nuse_tools: [raise_wrap]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe).await;

    let output = eval_one(&fixture.ctx, "raise_wrap", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    assert!(
        output["error"].as_str().unwrap().contains("kaboom"),
        "the exception must be diagnosable: {output}"
    );
    Ok(())
}

/// Fixture exposing one Python script tool with a raised timeout, selected
/// by its own name. The budget-cap tests share this scaffold.
async fn timed_python_tool_fixture(name: &str, body: &str, tools: Value) -> (Fixture, Arc<Probe>) {
    let mut tool = python_tool(name, body, tools);
    tool["timeout"] = json!("300s");
    let config = test_config(
        &[tool],
        &format!("---\nuse_tools: [{name}]\n---\n"),
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;
    (fixture, probe)
}

/// Python `tools` calls route through the same `InnerCallRun` as the Bash
/// commands, so the 1000-call budget applies; the concurrency cap holds by
/// the same construction (one shared semaphore per run, decision 11). Monty
/// is single-threaded, so the budget is the observable bound here.
#[tokio::test(flavor = "multi_thread")]
async fn python_calls_share_the_call_budget() -> Result<()> {
    let body = r#"i = 0
while i < 1005:
    tools.probe_run()
    i = i + 1"#;
    let (fixture, probe) = timed_python_tool_fixture("loop_wrap", body, json!(["probe_run"])).await;

    let output = eval_one(&fixture.ctx, "loop_wrap", json!({"number": 1})).await;
    assert!(
        is_error(&output),
        "the run must fail once the budget is spent: {output}"
    );
    let error = output["error"].as_str().expect("error message");
    assert!(error.contains("probe_run"), "tool name expected: {error}");
    assert!(
        error.contains("exceeded its limit"),
        "the budget rejection must be named: {error}"
    );
    assert_eq!(
        probe.call_count(),
        1000,
        "exactly the 1000-call budget may be dispatched"
    );
    Ok(())
}

/// A terminal fatal inner call stops the Python run: `try`/`except` cannot
/// swallow it and keep calling tools (CodeRabbit 4659980781). The attempt
/// cap is the deterministic Fatal trigger — rejections stay catchable, the
/// cap's Fatal does not.
#[tokio::test(flavor = "multi_thread")]
async fn python_fatal_inner_call_stops_the_run_despite_except() -> Result<()> {
    let body = r#"for i in range(20000):
    try:
        tools.probe_run()
    except RuntimeError:
        pass
tools.probe_run(value="must-not-run")"#;
    let (fixture, probe) = timed_python_tool_fixture("spin_wrap", body, json!(["probe_run"])).await;

    let output = eval_one(&fixture.ctx, "spin_wrap", json!({"number": 1})).await;
    assert!(
        is_error(&output),
        "the fatal must fail the tool call instead of resuming: {output}"
    );
    let error = output["error"].as_str().expect("error message");
    assert!(
        error.contains("stopped") && error.contains("tool-call attempts"),
        "the host stop and the attempt-cap fatal must be reported: {error}"
    );
    // The successful calls spend the 1000-call budget; the rejected ones
    // after it are what reach the attempt cap.
    assert_eq!(probe.call_count(), 1000);
    assert!(
        probe
            .calls()
            .iter()
            .all(|call| call.1["value"] != "must-not-run"),
        "the call after the except block must never dispatch"
    );
    Ok(())
}

/// Text/json output modes and the truncation marker behave exactly like the
/// Bash path (decision 7): stdout comes from `print`.
#[tokio::test(flavor = "multi_thread")]
async fn python_output_modes_and_truncation_match_the_bash_contract() -> Result<()> {
    let mut good = python_tool("good_json", "print(42)", json!([]));
    good["output"] = json!("json");
    let mut bad = python_tool("bad_json", "print('not-json')", json!([]));
    bad["output"] = json!("json");
    let mut big_text = python_tool(
        "big_text",
        r#"i = 0
while i < 210:
    print("x" * 5000, end="")
    i = i + 1"#,
        json!([]),
    );
    big_text["timeout"] = json!("30s");
    // C2: a complete JSON value followed by overflow must still fail — the
    // cap cut the stream, so the value is not the output.
    let mut valid_json = python_tool(
        "valid_json",
        r#"print('{"ok": true}', end="")
i = 0
while i < 210:
    print("x" * 5000, end="")
    i = i + 1"#,
        json!([]),
    );
    valid_json["output"] = json!("json");
    valid_json["timeout"] = json!("30s");
    let config = test_config(
        &[good, bad, big_text, valid_json],
        "---\nuse_tools: [good_json, bad_json, big_text, valid_json]\n---\n",
        Vec::new(),
    );
    let fixture = fixture(config, Arc::new(Probe::default())).await;

    let output = eval_one(&fixture.ctx, "good_json", json!({"number": 1})).await;
    assert!(!is_error(&output), "{output}");
    assert_eq!(output, json!(42), "parsed stdout is the result");

    let output = eval_one(&fixture.ctx, "bad_json", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    assert!(
        output["error"].as_str().unwrap().contains("invalid JSON"),
        "{output}"
    );

    let output = eval_one(&fixture.ctx, "big_text", json!({"number": 1})).await;
    assert!(
        !is_error(&output),
        "truncated text output is still a success"
    );
    assert_eq!(output["stdout_truncated"], true, "{output}");
    assert!(
        display_text(&output).contains("[stdout truncated"),
        "the truncation marker must reach the model: {output}"
    );

    let output = eval_one(&fixture.ctx, "valid_json", json!({"number": 1})).await;
    assert!(
        is_error(&output),
        "a valid JSON prefix must not survive a truncation: {output}"
    );
    assert!(
        output["error"].as_str().unwrap().contains("capture cap"),
        "the failure must name truncation: {output}"
    );
    Ok(())
}

/// The per-run timeout stops a python body and cancels the pending inner
/// call (R11).
#[tokio::test(flavor = "multi_thread")]
async fn python_timeout_stops_the_script_and_cancels_inner_calls() -> Result<()> {
    let mut tool = python_tool(
        "hang_wrap",
        r#"tools.probe_run(hang=True)"#,
        json!(["probe_run"]),
    );
    tool["timeout"] = json!("1s");
    let config = test_config(&[tool], "---\nuse_tools: [hang_wrap]\n---\n", Vec::new());
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let started_at = Instant::now();
    let output = eval_one(&fixture.ctx, "hang_wrap", json!({"number": 1})).await;
    assert!(is_error(&output), "a timed-out run is an error: {output}");
    let error = output["error"].as_str().expect("error message");
    assert!(error.contains("timed out"), "{error}");
    assert!(
        started_at.elapsed() < Duration::from_secs(10),
        "the 1s timeout must bound the run"
    );
    tokio::time::timeout(Duration::from_secs(5), probe.dropped.notified())
        .await
        .expect("the pending inner call must observe cancellation");
    Ok(())
}

/// The caller's abort signal stops a python body and cancels pending inner
/// calls (R11).
#[tokio::test(flavor = "multi_thread")]
async fn python_abort_stops_the_script_and_cancels_inner_calls() -> Result<()> {
    let config = test_config(
        &[python_tool(
            "hang_wrap",
            r#"tools.probe_run(hang=True)"#,
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [hang_wrap]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let abort = create_abort_signal();
    let ctx = fixture.ctx.clone();
    let call = ToolCall::new(
        "hang_wrap".to_string(),
        json!({"number": 1}),
        Some("hang-1".to_string()),
        None,
    );
    let task_abort = abort.clone();
    let task = tokio::spawn(async move { eval_tool_calls(&ctx, vec![call], &task_abort).await });
    tokio::time::timeout(Duration::from_secs(5), probe.started.notified())
        .await
        .expect("the inner call must start before the abort");
    abort.set_ctrlc();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the aborted run must settle")?;
    assert!(
        result.is_err(),
        "abort must surface as a batch-terminal error"
    );
    tokio::time::timeout(Duration::from_secs(5), probe.dropped.notified())
        .await
        .expect("the pending inner call must observe the abort");
    Ok(())
}

/// A script tool that declares no tools has no `tools` object; its Python
/// behaves as it did before `tools` existed.
#[tokio::test(flavor = "multi_thread")]
async fn a_python_body_without_tools_has_no_tools() -> Result<()> {
    let config = test_config(
        &[
            python_tool("reads_tools", "print(tools)", json!([])),
            python_tool("ignores_tools", "print('fine')", json!([])),
        ],
        "---\nuse_tools: [reads_tools, ignores_tools]\n---\n",
        Vec::new(),
    );
    let fixture = fixture(config, Arc::new(Probe::default())).await;

    let output = eval_one(&fixture.ctx, "reads_tools", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    let error = output["error"].as_str().expect("error message");
    assert!(error.contains("NameError"), "{error}");
    assert!(error.contains("'tools'"), "{error}");

    let output = eval_one(&fixture.ctx, "ignores_tools", json!({"number": 1})).await;
    assert!(!is_error(&output), "{output}");
    assert_eq!(display_text(&output).trim(), "fine");
    Ok(())
}
