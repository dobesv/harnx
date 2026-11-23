//! Script tool e2e through the runtime's real wiring: config resolution →
//! accepted view → declarations → `ScriptToolProvider` → slot handoff →
//! BashKit run → inner-call admission → engine result.
//!
//! Parity tests run each inner-call failure through `bash` script mode and
//! through a script tool body and compare the body-observed exit code and
//! stderr, never a hard-coded value (decision 11).

#![cfg(unix)]

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use harnx_core::abort::{create_abort_signal, AbortSignal};
use harnx_core::instance::ServerScope;
use harnx_core::tool::{ToolCall, ToolError, ToolProvider, ToolProviderOutput};
use harnx_engine::tool::{eval_tool_calls, ToolEvalContext};
use harnx_runtime::bash_provider::BashEvalContextSlot;
use harnx_runtime::config::{
    agent::Agent, AgentConfig, Config, ConfigLock, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV,
};
use harnx_runtime::tool::{build_tool_eval_context, BuildToolEvalContextParams};
use serde_json::{json, Value};
use tokio::sync::Notify;

const TOKEN: &str = "script-tools-test-token";

// ---------------------------------------------------------------------
// In-process probe tool: the base tool every script tool wraps.
// ---------------------------------------------------------------------

struct Probe {
    calls: parking_lot::Mutex<Vec<(String, Value)>>,
    concurrent: AtomicUsize,
    max_concurrent: AtomicUsize,
    started: Notify,
    dropped: Notify,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            calls: parking_lot::Mutex::new(Vec::new()),
            concurrent: AtomicUsize::new(0),
            max_concurrent: AtomicUsize::new(0),
            started: Notify::new(),
            dropped: Notify::new(),
        }
    }
}

impl Probe {
    fn record_concurrency(&self) -> ProbeGuard<'_> {
        let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent.fetch_max(now, Ordering::SeqCst);
        ProbeGuard(self)
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().clone()
    }

    fn call_count(&self) -> usize {
        self.calls.lock().len()
    }
}

struct ProbeGuard<'a>(&'a Probe);

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        self.0.concurrent.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ToolProvider for Probe {
    fn name(&self) -> &str {
        "probe"
    }

    fn has_tool(&self, name: &str) -> bool {
        matches!(name, "probe_run" | "probe_aux")
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        _abort: &AbortSignal,
    ) -> std::result::Result<ToolProviderOutput, ToolError> {
        self.calls.lock().push((name.to_string(), args.clone()));
        let _concurrency = self.record_concurrency();
        self.started.notify_one();
        if args["hang"] == true {
            // Dropping the run's interpreter futures is the cancellation the
            // timeout/abort tests observe through `dropped`.
            struct Hang<'a>(&'a Probe);
            impl Drop for Hang<'_> {
                fn drop(&mut self) {
                    self.0.dropped.notify_one();
                }
            }
            let _hang = Hang(self);
            std::future::pending::<()>().await;
        }
        if let Some(ms) = args["delay_ms"].as_u64() {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        if args["recoverable"] == true {
            return Err(ToolError::Recoverable(anyhow::anyhow!(
                "recoverable probe failure"
            )));
        }
        if args["fatal"] == true {
            return Err(ToolError::Fatal(anyhow::anyhow!("fatal probe failure")));
        }
        Ok(ToolProviderOutput::new(args))
    }
}

// ---------------------------------------------------------------------
// Config + context fixtures.
// ---------------------------------------------------------------------

fn probe_declaration(name: &str) -> harnx_core::tool::ToolDeclaration {
    serde_json::from_value(json!({
        "name": name,
        "description": "Probe tool",
        "parameters": {
            "type": "object",
            "properties": {
                "value": {"type": "string"},
                "number": {"type": "integer"},
                "delay_ms": {"type": "integer"},
                "recoverable": {"type": "boolean"},
                "fatal": {"type": "boolean"},
                "hang": {"type": "boolean"},
                "timezone": {"type": "string"}
            },
            "additionalProperties": false
        }
    }))
    .unwrap()
}

fn script_tool(name: &str, body: &str, tools: Value) -> Value {
    json!({
        "name": name,
        "description": "Wrapper.",
        "parameters": {
            "type": "object",
            "properties": {"number": {"type": "integer"}, "timezone": {"type": "string"}},
            "required": ["number"]
        },
        "tools": tools,
        "bash": body
    })
}

/// Same shape as [`script_tool`] with a Monty Python body instead of Bash.
fn python_tool(name: &str, body: &str, tools: Value) -> Value {
    let mut tool = script_tool(name, "", tools);
    tool.as_object_mut().unwrap().remove("bash");
    tool["python"] = json!(body);
    tool
}

mod python;

/// Config with seeded base-tool declarations, an agent, optional middleware
/// entries and one or more script tool definitions.
fn test_config(
    script_definitions: &[Value],
    agent_markdown: &str,
    middleware: Vec<Value>,
) -> Config {
    let definitions: Vec<harnx_core::script_tool::ScriptToolDefinition> = script_definitions
        .iter()
        .map(|value| serde_json::from_value(value.clone()).unwrap())
        .collect();
    let mut config = Config {
        data: harnx_core::config_data::ConfigData {
            middleware: serde_json::from_value(json!(middleware)).unwrap(),
            ..Default::default()
        },
        ..Config::default()
    };
    config
        .set_script_tool_definitions(&definitions)
        .expect("test script tools resolve");
    config.agent = Some(Agent::new(
        AgentConfig::from_markdown("worker", agent_markdown).unwrap(),
    ));
    *config.nats_tool_declarations.write() = ["probe_run", "probe_aux"]
        .iter()
        .map(|name| probe_declaration(name))
        .collect();
    config
}

struct Fixture {
    config: Arc<ConfigLock>,
    ctx: Arc<ToolEvalContext>,
}

/// Build the real round context with the probe injected as a base provider,
/// then hand the context to the shared slot exactly like a tool round does.
async fn fixture(config: Config, probe: Arc<Probe>) -> Fixture {
    fixture_with_instance(Arc::new(ConfigLock::new(config)), probe, ServerScope::new()).await
}

/// Same as [`fixture`], for tests that start tool servers first and refresh
/// declarations under a known instance scope before the context is built.
async fn fixture_with_instance(
    config: Arc<ConfigLock>,
    probe: Arc<Probe>,
    instance_id: ServerScope,
) -> Fixture {
    let slot = BashEvalContextSlot::new();
    let mut params = BuildToolEvalContextParams::new(&config, &instance_id);
    params.bash_context_slot = Some(slot.clone());
    params.extra_base_providers = vec![probe.clone() as Arc<dyn ToolProvider>];
    let ctx = Arc::new(build_tool_eval_context(params).await);
    slot.set(&ctx);
    Fixture { config, ctx }
}

async fn eval_one(ctx: &ToolEvalContext, name: &str, arguments: Value) -> Value {
    let mut results = eval_tool_calls(
        ctx,
        vec![ToolCall::new(
            name.to_string(),
            arguments,
            Some(format!("{name}-1")),
            None,
        )],
        &create_abort_signal(),
    )
    .await
    .expect("tool round evaluates");
    assert_eq!(results.len(), 1);
    results.remove(0).output
}

fn display_text(output: &Value) -> String {
    harnx_core::tool::extract_user_display_text(output)
        .unwrap_or_else(|| serde_json::to_string_pretty(output).unwrap())
}

fn is_error(output: &Value) -> bool {
    output["is_error"] == true
}

// ---------------------------------------------------------------------
// Catalog (R3, R4, decision 6, 12).
// ---------------------------------------------------------------------

/// Literal `use_tools` selection: the declaration carries the definition's
/// schema, the call runs the body, `tool_input` feeds it the arguments and
/// the inner call's output reaches the script (R5, R6).
#[tokio::test(flavor = "multi_thread")]
async fn script_tool_is_selectable_by_literal_name_and_runs() -> Result<()> {
    let config = test_config(
        &[script_tool(
            "wrap_probe",
            r#"n=$(tool_input | jq -r .number)
probe_run --value "issue-$n""#,
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [wrap_probe]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let declaration = fixture
        .ctx
        .render
        .as_ref()
        .expect("render context")
        .decl_map
        .get("wrap_probe")
        .expect("use_tools literal must expose the script tool");
    assert_eq!(
        declaration.parameters.as_value()["required"],
        json!(["number"]),
        "the definition's schema must be the advertised schema"
    );
    assert!(fixture.ctx.allowed_tool_names.contains("wrap_probe"));

    let output = eval_one(&fixture.ctx, "wrap_probe", json!({"number": 2813})).await;
    let text = display_text(&output);
    assert!(
        text.contains("issue-2813"),
        "inner call output must reach the script: {output}"
    );
    assert_eq!(probe.calls().len(), 1, "exactly one inner dispatch");
    Ok(())
}

/// Glob and toolset selectors expose script tools like any other tool (R3).
#[tokio::test(flavor = "multi_thread")]
async fn script_tools_are_selectable_by_glob_and_toolset() -> Result<()> {
    for (agent_md, name) in [
        ("---\nuse_tools: [wrap_*]\n---\n", "wrap_probe"),
        ("---\nuse_tools: [wrappers]\n---\n", "wrap_probe"),
    ] {
        let mut config = test_config(
            &[script_tool(
                "wrap_probe",
                "probe_run --value ok",
                json!(["probe_run"]),
            )],
            agent_md,
            Vec::new(),
        );
        config
            .toolsets
            .insert("wrappers".to_string(), vec!["wrap_probe".to_string()]);
        let probe = Arc::new(Probe::default());
        let fixture = fixture(config, probe.clone()).await;
        assert!(
            fixture
                .ctx
                .render
                .as_ref()
                .expect("render context")
                .decl_map
                .contains_key(name),
            "selector {agent_md:?} must expose {name}"
        );
        let output = eval_one(&fixture.ctx, name, json!({"number": 1})).await;
        assert!(!is_error(&output), "{output}");
        assert_eq!(probe.call_count(), 1);
    }
    Ok(())
}

/// `bash_tools` admits a script tool as a shell command (R4): the bash
/// provider's inner set contains it and dispatches to the script provider.
#[tokio::test(flavor = "multi_thread")]
async fn bash_script_mode_can_call_a_script_tool() -> Result<()> {
    let config = test_config(
        &[script_tool(
            "wrap_probe",
            "probe_run --value \"wrapped\"",
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [bash]\nbash_tools: [wrap_probe]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    assert!(fixture.ctx.providers[0].has_tool("bash"));
    let output = eval_one(
        &fixture.ctx,
        "bash",
        json!({"script": "wrap_probe --number 7"}),
    )
    .await;
    assert_eq!(output["status"], "ok", "{}", output);
    assert_eq!(output["exit_code"], 0);
    let calls = probe.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "probe_run");
    assert_eq!(calls[0].1["value"], "wrapped");
    Ok(())
}

/// A base-tool name collision removes the script tool from declarations and
/// dispatch at once; the original tool keeps the name (decision 6, 12).
#[tokio::test(flavor = "multi_thread")]
async fn base_tool_collision_disables_the_script_tool() -> Result<()> {
    let config = test_config(
        // The name collides with the seeded base-tool declaration
        // `probe_run`; the body must then call a different tool.
        &[script_tool(
            "probe_run",
            "probe_aux --value shadow",
            json!(["probe_aux"]),
        )],
        "---\nuse_tools: [probe_run]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    assert!(
        fixture.config.read().accepted_script_tools().is_empty(),
        "collision must drop the script tool from the accepted view"
    );
    let script_provider = fixture
        .ctx
        .providers
        .iter()
        .find(|provider| provider.name() == "script-tools")
        .expect("script tool provider is wired");
    assert!(!script_provider.has_tool("probe_run"));
    let declaration = fixture
        .ctx
        .render
        .as_ref()
        .expect("render context")
        .decl_map
        .get("probe_run")
        .expect("the original declaration stays");
    assert_eq!(
        declaration.description, "Probe tool",
        "the original tool must own the name"
    );

    let output = eval_one(&fixture.ctx, "probe_run", json!({"value": "direct"})).await;
    assert!(!is_error(&output), "the original stays callable: {output}");
    let calls = probe.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["value"], "direct");
    Ok(())
}

/// Original tools stay callable next to a wrapper that selects them (R4's
/// "originals remain callable").
#[tokio::test(flavor = "multi_thread")]
async fn original_tools_remain_callable_alongside_the_wrapper() -> Result<()> {
    let config = test_config(
        &[script_tool(
            "wrap_probe",
            "probe_run --value wrapped",
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [wrap_probe, probe_run]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let direct = eval_one(&fixture.ctx, "probe_run", json!({"value": "direct"})).await;
    assert!(!is_error(&direct), "{direct}");
    let wrapped = eval_one(&fixture.ctx, "wrap_probe", json!({"number": 1})).await;
    assert!(!is_error(&wrapped), "{wrapped}");

    let calls = probe.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1["value"], "direct");
    assert_eq!(calls[1].1["value"], "wrapped");
    Ok(())
}

// ---------------------------------------------------------------------
// Contract (R7, R8).
// ---------------------------------------------------------------------

/// Invalid arguments fail against the definition's schema and never run the
/// script (R8).
#[tokio::test(flavor = "multi_thread")]
async fn schema_invalid_arguments_fail_without_running_the_script() -> Result<()> {
    let config = test_config(
        &[script_tool(
            "wrap_probe",
            "probe_run --value ran",
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [wrap_probe]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "wrap_probe", json!({"number": "x"})).await;
    assert!(is_error(&output), "{output}");
    assert!(
        output["error"]
            .as_str()
            .expect("error message")
            .contains("invalid arguments"),
        "the schema violation must be named: {output}"
    );
    assert!(
        probe.calls().is_empty(),
        "the script must not run on invalid arguments"
    );
    Ok(())
}

/// Non-zero exit is an error with the exit status and stderr; exit 0 with
/// stderr is a success that still shows the stderr (R7).
#[tokio::test(flavor = "multi_thread")]
async fn exit_code_and_stderr_follow_the_output_contract() -> Result<()> {
    let config = test_config(
        &[
            script_tool("fail_wrap", "echo boom >&2; exit 3", json!([])),
            script_tool("warn_wrap", "echo out; echo warning >&2", json!([])),
        ],
        "---\nuse_tools: [fail_wrap, warn_wrap]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe).await;

    let output = eval_one(&fixture.ctx, "fail_wrap", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    let error = output["error"].as_str().expect("error message");
    assert!(error.contains("exit"), "exit status expected: {error}");
    assert!(error.contains("code 3"), "exit code expected: {error}");
    assert!(error.contains("boom"), "bounded stderr expected: {error}");
    assert!(
        error.contains("partial_result") || output["partial_result"].is_object(),
        "diagnostics ride along: {output}"
    );

    let output = eval_one(&fixture.ctx, "warn_wrap", json!({"number": 1})).await;
    assert!(!is_error(&output), "stderr alone must not fail: {output}");
    assert_eq!(output["stderr"], "warning\n");
    assert_eq!(display_text(&output).trim(), "out");
    Ok(())
}

/// `output: json` returns the parsed stdout value and rejects anything that
/// is not exactly one JSON value (decision 7).
#[tokio::test(flavor = "multi_thread")]
async fn json_output_mode_parses_exactly_one_value() -> Result<()> {
    let mut good = script_tool("good_json", "tool_input", json!([]));
    good["output"] = json!("json");
    let mut bad = script_tool("bad_json", "echo not-json", json!([]));
    bad["output"] = json!("json");
    let config = test_config(
        &[good, bad],
        "---\nuse_tools: [good_json, bad_json]\n---\n",
        Vec::new(),
    );
    let fixture = fixture(config, Arc::new(Probe::default())).await;

    let output = eval_one(&fixture.ctx, "good_json", json!({"number": 2813})).await;
    assert_eq!(
        output,
        json!({"number": 2813}),
        "parsed stdout is the result"
    );

    let output = eval_one(&fixture.ctx, "bad_json", json!({"number": 1})).await;
    assert!(is_error(&output), "{output}");
    assert!(
        output["error"].as_str().unwrap().contains("invalid JSON"),
        "{output}"
    );
    Ok(())
}

// ---------------------------------------------------------------------
// Inner-call exclusions (R10, decision 5).
// ---------------------------------------------------------------------

/// Non-allowlisted tools, `bash`, control tools and other script tools are
/// refused at dispatch, including when a glob matches them.
#[tokio::test(flavor = "multi_thread")]
async fn inner_calls_refuse_excluded_tools() -> Result<()> {
    let config = test_config(
        &[
            // `probe_*` also matches the second script tool's name; the glob
            // must skip it, not call it.
            script_tool(
                "glob_wrap",
                r#"set +e
probe_helper >/dev/null 2>/tmp/e1; echo "helper=$?"
probe_aux >/dev/null 2>/dev/null; echo "aux=$?"
ask_user --message hi >/dev/null 2>/dev/null; echo "ask=$?"
probe_run --value allowed >/dev/null 2>/dev/null; echo "run=$?"
cat /tmp/e1"#,
                json!(["probe_*"]),
            ),
            script_tool(
                "probe_helper",
                "probe_run --value helper",
                json!(["probe_run"]),
            ),
        ],
        "---\nuse_tools: [glob_wrap, probe_helper]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "glob_wrap", json!({"number": 1})).await;
    let text = display_text(&output);
    assert!(
        text.contains("helper=127"),
        "a script tool the glob matches must not be callable: {output}"
    );
    assert!(
        text.contains("aux=0"),
        "an allowlisted base tool matched by the glob stays callable: {output}"
    );
    assert!(
        text.contains("ask=127"),
        "control tools stay refused even under a matching glob: {output}"
    );
    assert!(
        text.contains("run=0"),
        "the allowlisted tool runs: {output}"
    );
    assert!(
        text.contains("no such command") || text.contains("not found") || text.contains("unknown"),
        "the refusal must name the command: {text}"
    );
    // Only the two allowlisted inner calls ran: probe_aux and probe_run from
    // glob_wrap's own body. probe_helper's body never executed.
    assert_eq!(probe.call_count(), 2, "probe calls: {:?}", probe.calls());
    Ok(())
}

// ---------------------------------------------------------------------
// Parity with bash script mode (R9, decision 11).
// ---------------------------------------------------------------------

fn extract_bash_stdout(output: &Value) -> String {
    if let Some(stdout) = output["stdout"].as_str() {
        return stdout.to_string();
    }
    let text = display_text(output);
    text.split("<!-- start stdout -->\n```\n")
        .nth(1)
        .and_then(|rest| rest.split("\n```\n<!-- end stdout -->").next())
        .expect("bash stdout block")
        .to_string()
}

/// Runs one inner-call scenario twice — once inside a `bash` script and once
/// inside a script tool body — and compares what the script observed. The
/// body captures the exit code and stderr of the inner call itself, so the
/// assertion compares Bash classification, not hard-coded codes.
async fn parity_case(middleware: Vec<Value>, inner: &str) -> Result<(String, String)> {
    let body = format!(
        r#"set +e
{inner} 2>/tmp/parity_err
echo "rc=$?"
cat /tmp/parity_err"#
    );
    let config = test_config(
        &[script_tool("parity_wrap", &body, json!(["probe_run"]))],
        "---\nuse_tools: [parity_wrap, bash]\nbash_tools: [probe_run]\n---\n",
        middleware,
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe).await;

    let via_bash = eval_one(&fixture.ctx, "bash", json!({"script": body})).await;
    let bash_stdout = extract_bash_stdout(&via_bash);

    let via_script = eval_one(&fixture.ctx, "parity_wrap", json!({"number": 1})).await;
    let script_stdout = display_text(&via_script);
    Ok((bash_stdout, script_stdout))
}

#[tokio::test(flavor = "multi_thread")]
async fn inner_call_parity_with_bash_script_mode() -> Result<()> {
    // An inner tool error result (a recoverable provider failure).
    let (bash, script) = parity_case(Vec::new(), "probe_run --recoverable").await?;
    assert_eq!(
        bash.trim(),
        script.trim(),
        "script tools must classify inner tool errors exactly like bash script mode"
    );

    // A middleware denial: agent middleware observes inner calls from both
    // paths and yields the same refusal.
    let deny = json!({"name": "deny-probe", "match": "^probe_run$", "bash": "deny 'not allowed'"});
    let (bash, script) = parity_case(vec![deny], "probe_run").await?;
    assert_eq!(bash.trim(), script.trim());
    assert!(
        bash.contains("not allowed"),
        "the denial reason must reach the script: {bash}"
    );

    // An inner call requiring interaction: inside a script the confirmation
    // turns into a denial, exactly as in bash script mode.
    let confirm = json!({"name": "confirm-probe", "match": "^probe_run$", "bash": "confirm --id gate 'sure?'"});
    let (bash, script) = parity_case(vec![confirm], "probe_run").await?;
    assert_eq!(bash.trim(), script.trim());
    assert!(
        bash.contains("interaction required"),
        "the interaction denial must be visible: {bash}"
    );
    Ok(())
}

// ---------------------------------------------------------------------
// Bounds (R11).
// ---------------------------------------------------------------------

/// More than 8 parallel inner calls never put more than 8 in flight.
#[tokio::test(flavor = "multi_thread")]
async fn inner_call_concurrency_is_capped_at_eight() -> Result<()> {
    let body = r#"for i in $(seq 1 12); do
  probe_run --delay-ms 150 &
done
wait"#;
    let config = test_config(
        &[script_tool("fan_out", body, json!(["probe_run"]))],
        "---\nuse_tools: [fan_out]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "fan_out", json!({"number": 1})).await;
    assert!(!is_error(&output), "{output}");
    let max = probe.max_concurrent.load(Ordering::SeqCst);
    assert_eq!(probe.call_count(), 12);
    assert!(
        max > 1,
        "the fan-out must actually run in parallel (max {max})"
    );
    assert!(max <= 8, "concurrency cap is 8, saw {max} in flight");
    Ok(())
}

/// Calls beyond the per-run budget are rejected and never dispatched.
#[tokio::test(flavor = "multi_thread")]
async fn inner_calls_beyond_the_budget_are_not_dispatched() -> Result<()> {
    let body = "for i in $(seq 1 1005); do probe_run; done";
    let config = test_config(
        &[script_tool("loop_wrap", body, json!(["probe_run"]))],
        "---\nuse_tools: [loop_wrap]\n---\n",
        Vec::new(),
    );
    let probe = Arc::new(Probe::default());
    let fixture = fixture(config, probe.clone()).await;

    let output = eval_one(&fixture.ctx, "loop_wrap", json!({"number": 1})).await;
    assert!(
        is_error(&output),
        "the run must fail once the budget is spent: {output}"
    );
    assert_eq!(
        probe.call_count(),
        1000,
        "exactly the 1000-call budget may be dispatched"
    );
    Ok(())
}

/// Output past the capture cap: text mode keeps a truncation marker, json
/// mode errors instead of returning partial JSON (decision 7). The json
/// case must fail even when the retained stdout starts with a complete
/// JSON value (C2): the cap cut the stream, so the value is not the output.
#[tokio::test(flavor = "multi_thread")]
async fn stdout_past_the_cap_marks_text_and_fails_json() -> Result<()> {
    // BashKit bounds printf field widths and builtin output sizes, so build
    // the oversized stream in 5000-byte chunks (same trick as the bash
    // provider tests); 210 chunks exceed the 1 MiB stdout cap.
    let body = "for i in $(seq 1 210); do printf '%05000d' 0; done";
    let valid_prefix =
        r#"printf '{"ok": true}'; for i in $(seq 1 210); do printf '%05000d' 0; done"#;
    let mut text_tool = script_tool("big_text", body, json!([]));
    text_tool["timeout"] = json!("30s");
    let mut json_tool = script_tool("big_json", body, json!([]));
    json_tool["output"] = json!("json");
    json_tool["timeout"] = json!("30s");
    let mut valid_json_tool = script_tool("valid_json", valid_prefix, json!([]));
    valid_json_tool["output"] = json!("json");
    valid_json_tool["timeout"] = json!("30s");
    let config = test_config(
        &[text_tool, json_tool, valid_json_tool],
        "---\nuse_tools: [big_text, big_json, valid_json]\n---\n",
        Vec::new(),
    );
    let fixture = fixture(config, Arc::new(Probe::default())).await;

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

    let output = eval_one(&fixture.ctx, "big_json", json!({"number": 1})).await;
    assert!(
        is_error(&output),
        "truncated stdout is not valid JSON: {output}"
    );

    let output = eval_one(&fixture.ctx, "valid_json", json!({"number": 1})).await;
    assert!(
        is_error(&output),
        "a valid JSON prefix must not survive a truncation: {output}"
    );
    let error = output["error"].as_str().expect("error message");
    assert!(
        error.contains("capture cap"),
        "the failure must name truncation: {error}"
    );
    Ok(())
}

/// N1: hooks, operator `.call` and reservation contexts have no round
/// context slot, so script tool bodies could never run there. Those
/// contexts must not advertise script tools: the declaration map and the
/// script provider both stay without them.
#[tokio::test(flavor = "multi_thread")]
async fn script_tools_are_absent_from_contexts_without_an_evaluation_slot() -> Result<()> {
    let config = test_config(
        &[script_tool(
            "wrap_probe",
            "probe_run --value wrapped",
            json!(["probe_run"]),
        )],
        "---\nuse_tools: [wrap_probe]\n---\n",
        Vec::new(),
    );
    let config = Arc::new(ConfigLock::new(config));

    let scope = ServerScope::new();
    let params =
        BuildToolEvalContextParams::new(&config, &scope).with_agent_use_tools(Some("wrap_probe"));
    let ctx = build_tool_eval_context(params).await;
    assert!(
        !ctx.render
            .as_ref()
            .expect("render context")
            .decl_map
            .contains_key("wrap_probe"),
        "a slot-less context must not advertise the script tool"
    );
    let script_provider = ctx
        .providers
        .iter()
        .find(|provider| provider.name() == "script-tools")
        .expect("script tool provider is wired even without a slot");
    assert!(
        !script_provider.has_tool("wrap_probe"),
        "dispatch must not carry the script tool either"
    );
    Ok(())
}

/// The per-run timeout stops the script and cancels the pending inner call.
#[tokio::test(flavor = "multi_thread")]
async fn timeout_stops_the_script_and_cancels_inner_calls() -> Result<()> {
    let mut tool = script_tool("hang_wrap", "probe_run --hang", json!(["probe_run"]));
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

/// The caller's abort signal stops the script and cancels pending inner
/// calls, surfacing as a batch-terminal error.
#[tokio::test(flavor = "multi_thread")]
async fn abort_stops_the_script_and_cancels_inner_calls() -> Result<()> {
    let config = test_config(
        &[script_tool(
            "hang_wrap",
            "probe_run --hang",
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

// ---------------------------------------------------------------------
// Cold start (decision 13): a script tool selected through `use_tools`
// alone must cause the managed tool server behind its `tools:` selector to
// start, and the wrapper's inner call then succeeds.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn selected_script_tool_starts_its_managed_tool_server() -> Result<()> {
    use crate::bash_script_mode::{start_time_server, time_server_binary, time_server_config};

    harnx_core::require_nextest();
    let Some(server) =
        crate::common::spawn_nats_server_with_options(crate::common::SpawnNatsServerOptions {
            auth_token: Some(TOKEN.to_string()),
        })
        .await?
    else {
        return Ok(());
    };
    // Env guard first, binary second: the exact statement order is the
    // fixture's identity for the DRY checker, and this test's sequence
    // differs from the bash script-mode e2es on purpose.
    let _env = crate::bash_script_mode::EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let binary = time_server_binary()?.to_string_lossy().into_owned();

    // The agent selects only the script tool; the time server behind the
    // wrapper's `tools:` selector must still be selected for startup
    // (decision 13), or a cold-started wrapper has nothing to call.
    let wrap_time = script_tool(
        "wrap_time",
        "time_get_current_time --timezone UTC",
        json!(["time_get_current_time"]),
    );
    let mut config = test_config(
        &[wrap_time],
        "---\nuse_tools: [wrap_time]\n---\n",
        Vec::new(),
    );
    config.tool_servers = vec![time_server_config(&binary)];

    let config = Arc::new(ConfigLock::new(config));
    // The same server selection worker startup and session reconciliation
    // apply: `use_tools: [wrap_time]` alone must pull in the wrapper's
    // `tools:` selector.
    let selected = harnx_runtime::nats_worker::configured_worker_services_for_test(&config);
    assert_eq!(
        selected.len(),
        1,
        "only the wrapped tool's server must start, got {selected:?}"
    );
    assert_eq!(selected[0].name, "time");

    let instance_id = ServerScope::new();
    let supervisor = start_time_server(server.url(), TOKEN, &instance_id, &selected).await?;

    harnx_runtime::tool_context::refresh_nats_tool_declarations(&config, &instance_id).await;
    let fixture = fixture_with_instance(config, Arc::new(Probe::default()), instance_id).await;
    assert!(
        fixture
            .ctx
            .render
            .as_ref()
            .expect("render context")
            .decl_map
            .contains_key("wrap_time"),
        "the wrapper must be declared once the time server registered"
    );

    let output = eval_one(&fixture.ctx, "wrap_time", json!({"number": 1})).await;
    assert!(
        !is_error(&output),
        "the wrapper's inner call must succeed after cold start: {output}"
    );
    drop(supervisor);
    Ok(())
}
