//! `BashProvider` behavior: slot handoff, dispatch priority, replay, output
//! shaping, and hook interaction. Heavy executor-layer coverage lives in
//! `bash_inner`; these tests exercise the provider as the engine sees it.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    instance::ServerScope,
    partial_result::{error_with_partial_result, partial_result_of},
    tool::{
        ToolCall, ToolCallOrigin, ToolError, ToolProgress, ToolProvider, ToolProviderOutput,
        ToolReplay,
    },
};
use harnx_engine::tool::{eval_tool_calls, ToolEvalContext};
use serde_json::{json, Value};

use super::*;
use crate::bash_inner::MAX_RETAINED_TRACE_ENTRIES;
use crate::structured_provider::StructuredProvider;

mod python;
mod python_tools;

/// `ToolError` is not `Debug`, so tests unwrap results through these helpers
/// instead of `expect`/`expect_err`.
fn ok(result: Result<ToolProviderOutput, ToolError>) -> ToolProviderOutput {
    match result {
        Ok(output) => output,
        Err(ToolError::Recoverable(error)) => panic!("expected success, got {error:#}"),
        Err(ToolError::Fatal(error)) => panic!("expected success, got fatal {error:#}"),
    }
}

fn recoverable(result: Result<ToolProviderOutput, ToolError>, what: &str) -> anyhow::Error {
    match result {
        Err(ToolError::Recoverable(error)) => error,
        Err(ToolError::Fatal(error)) => panic!("expected {what}, got fatal {error:#}"),
        Ok(output) => panic!("expected {what}, got Ok({})", output.value),
    }
}

fn fatal(result: Result<ToolProviderOutput, ToolError>, what: &str) -> anyhow::Error {
    match result {
        Err(ToolError::Fatal(error)) => error,
        Err(ToolError::Recoverable(error)) => panic!("expected {what}, got {error:#}"),
        Ok(output) => panic!("expected {what}, got Ok({})", output.value),
    }
}

/// Fake inner tool provider. Records every call; optional gates let tests
/// hold calls in flight for timeout and abort coverage.
struct FakeProvider {
    calls: Mutex<Vec<(String, Value)>>,
    gate: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    release: Option<Arc<tokio::sync::Semaphore>>,
}

impl FakeProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            gate: None,
            release: None,
        })
    }

    fn gated(
        started: tokio::sync::mpsc::UnboundedSender<String>,
        release: Arc<tokio::sync::Semaphore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            gate: Some(started),
            release: Some(release),
        })
    }
}

#[async_trait::async_trait]
impl ToolProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        matches!(tool_name, "fs_read" | "slow" | "big" | "broken")
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.calls.lock().unwrap().push((name.into(), args.clone()));
        if let (Some(started), Some(release)) = (&self.gate, &self.release) {
            if name == "slow" {
                started.send(name.into()).unwrap();
                release.acquire().await.unwrap().forget();
            }
        }
        match name {
            "broken" => Err(ToolError::Recoverable(error_with_partial_result(
                anyhow!("fake tool failure"),
                json!({"partial": 1}),
            ))),
            "big" => Ok(ToolProviderOutput::new(json!({
                "content": [{"type": "text", "text": "x".repeat(2 * 1024 * 1024)}],
            }))),
            _ => Ok(ToolProviderOutput::new(json!({
                "content": [{"type": "text", "text": format!("{}:{}", name, args["path"].as_str().unwrap_or("?"))}],
            }))),
        }
    }
}

/// A raw external tool that also claims the `bash` name, like a NATS
/// raw-name alias. It must never win dispatch against `BashProvider`.
struct RawBashProvider;

#[async_trait::async_trait]
impl ToolProvider for RawBashProvider {
    fn name(&self) -> &str {
        "raw-bash"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        tool_name == BASH_TOOL_NAME
    }

    async fn call_tool(
        &self,
        _name: &str,
        _arguments: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        Ok(ToolProviderOutput::new(json!({"raw_bash": true})))
    }
}

struct CollectingProgress(Mutex<Vec<Value>>);

impl ToolProgress for CollectingProgress {
    fn update(&self, patch: harnx_core::tool::ToolUpdatePatch) {
        // Snapshot enough to assert updates flowed; full patch content is
        // covered by the InnerProgressAggregator tests.
        self.0
            .lock()
            .unwrap()
            .push(serde_json::to_value(&patch).unwrap_or(Value::Null));
    }
}

fn inner_declarations() -> Vec<harnx_core::tool::ToolDeclaration> {
    ["fs_read", "slow", "big", "broken"]
        .into_iter()
        .map(|name| harnx_core::tool::ToolDeclaration {
            name: name.to_string(),
            description: format!("Call {name}."),
            parameters: serde_json::from_value(json!({
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }))
            .unwrap(),
            mcp_tool_name: None,
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
            kind: None,
        })
        .collect()
}

/// Build a provider wired to a slot, the slot's context, and a hook event
/// collector. The context's provider list is the full dispatch order the
/// runtime produces: the `bash` provider first, then `extra` providers
/// (the raw alias), then the inner tool provider the script calls.
type Harness = (
    Arc<BashProvider>,
    Arc<BashEvalContextSlot>,
    Arc<ToolEvalContext>,
);

fn harness(inner: Arc<dyn ToolProvider>, extra: Vec<Arc<dyn ToolProvider>>) -> Harness {
    harness_with_attachments(inner, extra, None)
}

/// Same wiring as `harness`, but with the model-round payload shape.
fn model_harness(inner: Arc<dyn ToolProvider>, extra: Vec<Arc<dyn ToolProvider>>) -> Harness {
    build_harness(inner, extra, None, false)
}

fn harness_with_attachments(
    inner: Arc<dyn ToolProvider>,
    extra: Vec<Arc<dyn ToolProvider>>,
    fetcher: Option<Arc<dyn harnx_script::fs_attachments::AttachmentFetcher>>,
) -> Harness {
    build_harness(inner, extra, fetcher, true)
}

fn build_harness(
    inner: Arc<dyn ToolProvider>,
    extra: Vec<Arc<dyn ToolProvider>>,
    fetcher: Option<Arc<dyn harnx_script::fs_attachments::AttachmentFetcher>>,
    full_payload: bool,
) -> Harness {
    build_harness_with(
        inner,
        extra,
        fetcher,
        full_payload,
        (inner_declarations(), Default::default()),
    )
}

fn build_harness_with(
    inner: Arc<dyn ToolProvider>,
    extra: Vec<Arc<dyn ToolProvider>>,
    fetcher: Option<Arc<dyn harnx_script::fs_attachments::AttachmentFetcher>>,
    full_payload: bool,
    (declarations, script_shapes): (
        Vec<harnx_core::tool::ToolDeclaration>,
        std::collections::BTreeMap<String, crate::ScriptShape>,
    ),
) -> Harness {
    let slot = Arc::new(BashEvalContextSlot::default());
    let bash = Arc::new(
        BashProvider::new(BashProviderConfig {
            active: true,
            inner_declarations: declarations,
            base_providers: vec![inner.clone()],
            slot: Some(slot.clone()),
            payload_limit: None,
            script_shapes,
            full_payload,
        })
        .with_attachment_fetcher(fetcher),
    );
    let mut providers: Vec<Arc<dyn ToolProvider>> = vec![bash.clone()];
    providers.extend(extra);
    providers.push(inner);
    let ctx = Arc::new(ToolEvalContext {
        structured_results: false,
        on_tool_call_completed: None,
        on_interaction: None,
        middleware_fn: None,
        work_boundary: None,
        instance_id: ServerScope::new(),
        tool_round: Some(1),
        render: None,
        providers,
        allowed_tool_names: HashSet::new(),
        current_agent_package: None,
        handoff_targets: HashMap::new(),
        emit_tool_call_fn: Arc::new(|_, _| {}),
        emit_tool_result_fn: Arc::new(|_, _| {}),
        emit_tool_blocked_fn: Arc::new(|_, _| {}),
        emit_tool_update_fn: Arc::new(|_, _| {}),
        confirm_tool_use_fn: Arc::new(|_, _, _| panic!("inner calls must not confirm")),
        prepare_handoff_fn: None,
    });
    slot.set(&ctx);
    (bash, slot, ctx)
}

fn bash_call(script: &str) -> Value {
    json!({ "script": script })
}

async fn run(
    provider: &BashProvider,
    script: &str,
    abort: &AbortSignal,
) -> Result<ToolProviderOutput, ToolError> {
    provider
        .call_tool_with_progress(
            BASH_TOOL_NAME,
            bash_call(script),
            ToolCallOrigin {
                structured_result: false,
                tool_call_id: Some("outer-1"),
                tool_round: Some(1),
            },
            abort,
            Arc::new(CollectingProgress(Mutex::new(Vec::new()))),
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_round_payload_carries_output_once() {
    let (bash, _slot, _ctx) = model_harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let payload = ok(run(&bash, "echo marker-7; fs_read --path a > /dev/null", &abort).await).value;
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["exit_code"], 0);
    for key in ["stdout", "stderr", "trace"] {
        assert!(payload.get(key).is_none(), "{key} duplicated in {payload}");
    }
    assert_eq!(payload["stdout_truncated"], false);
    assert_eq!(
        payload.to_string().matches("marker-7").count(),
        1,
        "{payload}"
    );
    let text = payload["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("- fs_read ok"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_round_failure_keeps_output_in_the_partial_result_once() {
    let (bash, _slot, _ctx) = model_harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let error = recoverable(run(&bash, "echo marker-7; exit 3", &abort).await, "exit 3");
    let partial = partial_result_of(&error).expect("partial result");
    assert_eq!(partial["exit_code"], 3);
    assert!(partial.get("stdout").is_none(), "{partial}");
    assert_eq!(
        partial.to_string().matches("marker-7").count(),
        1,
        "{partial}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_tool_pipeline_and_reports_result() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let output = ok(run(&bash, "fs_read --path a | wc -l", &abort).await);
    let payload = output.value;
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["exit_code"], 0);
    assert_eq!(payload["trace"].as_array().map(Vec::len), Some(1));
    assert_eq!(payload["trace"][0]["name"], "fs_read");
    // The rendered text carries the YAML header plus the stdout block.
    let text = payload["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("status: ok"), "{text}");
    assert!(text.contains("<!-- start stdout -->"), "{text}");
    assert!(text.contains("1"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nonzero_exit_is_recoverable_with_partial_output_and_trace() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let error = recoverable(
        run(&bash, "fs_read --path a; broken --path x", &abort).await,
        "script failure",
    );
    let partial = partial_result_of(&error).expect("failure carries partial result");
    assert_eq!(partial["status"], "error");
    // Provider failures exit 1 in the script dialect; usage errors exit 2
    // (commands.rs mapping).
    assert_eq!(partial["exit_code"], 1);
    // The call before the failure still surfaces in stdout and the trace.
    assert!(partial["stdout"].as_str().unwrap().contains("fs_read:a"));
    assert_eq!(
        partial["trace"].as_array().map(Vec::len),
        Some(2),
        "{partial}"
    );
    assert_eq!(partial["failed_call"]["name"], "broken");
    assert_eq!(partial["failed_call"]["status"], "tool_error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_reports_partial_result() {
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let inner = FakeProvider::gated(started_tx, release.clone());
    let (bash, _slot, _ctx) = harness(inner, Vec::new());
    let abort = create_abort_signal();
    let task = tokio::spawn(async move {
        bash.call_tool_with_progress(
            BASH_TOOL_NAME,
            json!({"script": "fs_read --path a; slow --path b", "timeout_secs": 1}),
            ToolCallOrigin::default(),
            &abort,
            Arc::new(CollectingProgress(Mutex::new(Vec::new()))),
        )
        .await
    });
    started_rx.recv().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    release.add_permits(4);
    let error = recoverable(result, "timeout");
    let partial = partial_result_of(&error).expect("timeout carries partial result");
    assert_eq!(partial["status"], "timed_out");
    // The first call finished before the deadline; its output is preserved.
    assert!(partial["stdout"].as_str().unwrap().contains("fs_read:a"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_abort_is_fatal_with_partial_result() {
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let inner = FakeProvider::gated(started_tx, release.clone());
    let (bash, _slot, _ctx) = harness(inner, Vec::new());
    let abort = create_abort_signal();
    let task_abort = abort.clone();
    let task = tokio::spawn(async move {
        bash.call_tool_with_progress(
            BASH_TOOL_NAME,
            json!({"script": "fs_read --path a; slow --path b"}),
            ToolCallOrigin::default(),
            &task_abort,
            Arc::new(CollectingProgress(Mutex::new(Vec::new()))),
        )
        .await
    });
    started_rx.recv().await.unwrap();
    abort.set_ctrlc();
    release.add_permits(4);
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let error = fatal(result, "parent abort");
    let partial = partial_result_of(&error).expect("abort carries partial result");
    assert_eq!(partial["status"], "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn huge_stdout_spills_into_limit_marker() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let output = ok(run(&bash, "big --path x", &abort).await);
    let payload = output.value;
    // Render-time controls cannot recover bytes discarded by backend capture.
    assert_eq!(
        payload["stdout_truncated"], true,
        "2 MiB must exceed the 1 MiB stdout limit"
    );
    assert!(
        payload["limit_hit"].is_string(),
        "limit_hit must name the capped stream: {}",
        payload
    );
    let text = payload["content"][0]["text"].as_str().unwrap();
    output::assert_backend_capture_notice(text);
    assert!(text.len() < 2 * 1024 * 1024, "report must stay bounded");
}

/// PR3 review F10: the `bash_exec` header totals cover both streams, so a
/// stderr-only failure must not report 0/0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_totals_sum_stdout_and_stderr() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let error = recoverable(
        run(&bash, "echo out; echo err >&2; exit 3", &abort).await,
        "exit 3",
    );
    let payload = partial_result_of(&error).expect("failure carries partial result");
    assert_eq!(payload["exit_code"], 3);
    assert_eq!(payload["stdout"], "out\n");
    assert_eq!(payload["stderr"], "err\n");
    let text = payload["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("total_lines: 2"), "{text}");
    assert!(text.contains("total_bytes: 8"), "{text}");
}

/// PR3 review F13: inner calls past the retained-trace cap (256) have no
/// rows left, so the dropped count must surface in both the payload and the
/// rendered report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_trace_count_surfaces_in_result() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let script = "fs_read --path a\n".repeat(MAX_RETAINED_TRACE_ENTRIES + 4);
    let output = ok(run(&bash, &script, &abort).await);
    let payload = output.value;
    assert_eq!(
        payload["trace"].as_array().map(Vec::len),
        Some(MAX_RETAINED_TRACE_ENTRIES),
        "{payload}"
    );
    assert_eq!(payload["trace_dropped"], 4);
    let text = payload["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("4 further inner call(s)"), "{text}");
    let section = trace_section(&payload);
    assert!(
        section
            .trim_end()
            .ends_with("4 further inner call(s) beyond the retained-trace cap"),
        "{section}"
    );
}

fn trace_section(payload: &Value) -> String {
    let text = payload["content"][0]["text"].as_str().unwrap();
    text.split("## inner call trace\n")
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_calls_are_listed_and_eleven_are_grouped() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let ten = ok(run(&bash, &"fs_read --path a >/dev/null\n".repeat(10), &abort).await).value;
    assert_eq!(trace_section(&ten).lines().count(), 10, "{ten}");
    let eleven = ok(run(&bash, &"fs_read --path a >/dev/null\n".repeat(11), &abort).await).value;
    let section = trace_section(&eleven);
    assert_eq!(section.lines().count(), 1, "{section}");
    assert!(section.starts_with("- fs_read ×11 ok "), "{section}");
    // Operator payload keeps every row.
    assert_eq!(eleven["trace"].as_array().map(Vec::len), Some(11));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grouped_trace_names_the_first_failure() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    // The last command succeeds, so the script itself exits 0.
    let script = format!(
        "set +e\n{}broken --path x\n{}",
        "fs_read --path a >/dev/null\n".repeat(6),
        "fs_read --path a >/dev/null\n".repeat(5)
    );
    let payload = ok(run(&bash, &script, &abort).await).value;
    let section = trace_section(&payload);
    assert!(section.contains("- fs_read ×11 ok "), "{section}");
    assert!(section.contains("- broken ×1 tool_error "), "{section}");
    assert!(
        section.contains("first failure: broken tool_error "),
        "{section}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grouped_trace_keeps_one_line_per_tool_and_status_in_first_seen_order() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    // The failing tool is seen first, then a succeeding one, then the failing
    // one again: each (tool, status) still gets a single line, in the order
    // it first appeared.
    let script = format!(
        "set +e\nbroken --path x\n{}broken --path y\nbroken --path z\n{}",
        "fs_read --path a >/dev/null\n".repeat(6),
        "fs_read --path a >/dev/null\n".repeat(3)
    );
    let payload = ok(run(&bash, &script, &abort).await).value;
    let section = trace_section(&payload);
    let prefixes: Vec<String> = section
        .lines()
        .filter(|line| line.starts_with("- "))
        .map(|line| line.split(' ').take(4).collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        prefixes,
        ["- broken ×3 tool_error", "- fs_read ×9 ok"],
        "{section}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_slot_fails_closed() {
    let bash = BashProvider::new(BashProviderConfig {
        active: true,
        inner_declarations: inner_declarations(),
        base_providers: vec![],
        slot: None,
        payload_limit: None,
        script_shapes: Default::default(),
        full_payload: false,
    });
    let abort = create_abort_signal();
    let error = recoverable(run(&bash, "fs_read --path a", &abort).await, "no slot");
    assert!(
        error.to_string().contains(BASH_TOOL_NAME),
        "error should name the bash tool: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inactive_provider_does_not_claim_bash() {
    let bash = BashProvider::new(BashProviderConfig {
        active: false,
        inner_declarations: inner_declarations(),
        base_providers: vec![],
        slot: None,
        payload_limit: None,
        script_shapes: Default::default(),
        full_payload: false,
    });
    assert!(!bash.has_tool(BASH_TOOL_NAME));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_prefers_bash_script_provider_over_raw_alias() {
    // R2-2: with both providers in the context, the engine must route
    // `bash` to the script provider, not the raw external alias.
    let (_bash, _slot, ctx) = harness(FakeProvider::new(), vec![Arc::new(RawBashProvider)]);
    let abort = create_abort_signal();
    let results = eval_tool_calls(
        &ctx,
        vec![ToolCall {
            name: BASH_TOOL_NAME.to_string(),
            arguments: bash_call("fs_read --path a"),
            id: Some("call-1".to_string()),
            thought_signature: None,
            reasoning_provenance: None,
        }],
        &abort,
    )
    .await
    .expect("dispatch should evaluate");
    assert_eq!(results.len(), 1);
    let output = &results[0].output;
    assert!(
        output.get("raw_bash").is_none(),
        "raw alias won dispatch: {output}"
    );
    assert_eq!(output["status"], "ok");
    assert_eq!(output["trace"][0]["name"], "fs_read");
    // Double-check the harness itself routes through the same provider list
    // the runtime builds: BashProvider at index 0.
    assert!(ctx.providers[0].has_tool(BASH_TOOL_NAME));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_reports_interrupted_for_bash_only() {
    let bash = BashProvider::new(BashProviderConfig {
        active: true,
        inner_declarations: inner_declarations(),
        base_providers: vec![],
        slot: None,
        payload_limit: None,
        script_shapes: Default::default(),
        full_payload: false,
    });
    let abort = create_abort_signal();
    let replay_bash = match bash
        .replay_tool_call(
            ToolReplay {
                session_id: "s",
                tool_round: 1,
                call: &ToolCall {
                    name: BASH_TOOL_NAME.to_string(),
                    arguments: bash_call("fs_read --path a"),
                    id: None,
                    thought_signature: None,
                    reasoning_provenance: None,
                },
                worker_id: None,
                fence_token: None,
                authorization: None,
            },
            &abort,
        )
        .await
    {
        Ok(output) => output,
        Err(ToolError::Recoverable(error)) => panic!("replay failed: {error:#}"),
        Err(ToolError::Fatal(error)) => panic!("replay failed: {error:#}"),
    };
    let output = replay_bash.expect("bash replay must answer interrupted");
    assert_eq!(output.value["status"], "interrupted");

    let other = match bash
        .replay_tool_call(
            ToolReplay {
                session_id: "s",
                tool_round: 1,
                call: &ToolCall {
                    name: "fs_read".to_string(),
                    arguments: json!({"path": "a"}),
                    id: None,
                    thought_signature: None,
                    reasoning_provenance: None,
                },
                worker_id: None,
                fence_token: None,
                authorization: None,
            },
            &abort,
        )
        .await
    {
        Ok(output) => output,
        Err(ToolError::Recoverable(error)) => panic!("replay failed: {error:#}"),
        Err(ToolError::Fatal(error)) => panic!("replay failed: {error:#}"),
    };
    assert!(
        other.is_none(),
        "non-bash calls must fall through to other providers"
    );
}

/// PR3 review F9: with script mode inactive there is no script that could
/// have been running, so even a bare `bash` call must fall through to the
/// NATS provider's normal journal recovery instead of being answered as
/// `interrupted`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inactive_provider_replay_falls_through_for_bash() {
    let bash = BashProvider::new(BashProviderConfig {
        active: false,
        inner_declarations: inner_declarations(),
        base_providers: vec![],
        slot: None,
        payload_limit: None,
        script_shapes: Default::default(),
        full_payload: false,
    });
    let abort = create_abort_signal();
    let replay = bash
        .replay_tool_call(
            ToolReplay {
                session_id: "s",
                tool_round: 1,
                call: &ToolCall {
                    name: BASH_TOOL_NAME.to_string(),
                    arguments: bash_call("fs_read --path a"),
                    id: None,
                    thought_signature: None,
                    reasoning_provenance: None,
                },
                worker_id: None,
                fence_token: None,
                authorization: None,
            },
            &abort,
        )
        .await;
    let replay = match replay {
        Ok(output) => output,
        Err(ToolError::Recoverable(error)) => panic!("replay failed: {error:#}"),
        Err(ToolError::Fatal(error)) => panic!("replay failed: {error:#}"),
    };
    assert!(
        replay.is_none(),
        "inactive script mode must not claim bash replay"
    );
}

/// PR3 review F2: the slot holds the context weakly, so a dropped round must
/// surface as fail-closed, not as a stale-context dispatch. Dangling weak
/// references (round ended, context gone) behave exactly like a missing slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dangling_slot_context_fails_closed() {
    let (bash, slot, ctx) = harness(FakeProvider::new(), Vec::new());
    // Sanity: the weak reference upgrades while the round's context is alive.
    assert!(
        slot.get().is_some(),
        "weak slot reference must upgrade while the context is alive"
    );
    // End the round: dropping the last strong reference dangles the slot.
    drop(ctx);
    let abort = create_abort_signal();
    let error = recoverable(
        run(&bash, "fs_read --path a", &abort).await,
        "dangling slot",
    );
    let message = format!("{error:#}");
    assert!(
        message.contains("context"),
        "dangling slot must fail closed mentioning the context: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_updates_reach_outer_handle() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let progress = Arc::new(CollectingProgress(Mutex::new(Vec::new())));
    ok(bash
        .call_tool_with_progress(
            BASH_TOOL_NAME,
            bash_call("fs_read --path a"),
            ToolCallOrigin::default(),
            &abort,
            progress.clone(),
        )
        .await);
    let updates = progress.0.lock().unwrap();
    assert!(
        !updates.is_empty(),
        "inner activity must surface on the outer progress handle"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inner_calls_cannot_recurse_into_bash() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let error = recoverable(
        run(&bash, "bash --script 'fs_read --path a'", &abort).await,
        "unknown command",
    );
    let partial = partial_result_of(&error).expect("failure carries partial result");
    // `bash` is not registered as a script command; the run must not hang or
    // dispatch back into BashProvider. Unknown command exits 2 with no
    // dispatched inner call.
    assert_eq!(partial["exit_code"], 2, "{}", partial);
    assert!(
        partial["trace"]
            .as_array()
            .is_none_or(|trace| trace.is_empty()),
        "no inner call may be dispatched for a bare `bash` command: {}",
        partial
    );
}

/// #2630 command-scoped catalog: the provider stays active with an empty
/// inner set (pure shell), unknown tool names are never dispatched, and the
/// slot's installed owner keeps the weak reference alive for exactly the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn command_scoped_catalog_stays_active_and_fails_closed_without_tools() {
    let slot = Arc::new(BashEvalContextSlot::default());
    let bash = Arc::new(BashProvider::new(BashProviderConfig {
        // Command-scoped (#2630): active even though the catalog is empty.
        active: true,
        inner_declarations: Vec::new(),
        base_providers: Vec::new(),
        slot: Some(slot.clone()),
        payload_limit: None,
        script_shapes: Default::default(),
        full_payload: false,
    }));
    assert!(bash.has_tool(BASH_TOOL_NAME));

    let ctx = slot.install(ToolEvalContext {
        structured_results: false,
        on_tool_call_completed: None,
        on_interaction: None,
        middleware_fn: None,
        work_boundary: None,
        instance_id: ServerScope::new(),
        tool_round: None,
        render: None,
        providers: vec![bash.clone()],
        allowed_tool_names: HashSet::new(),
        current_agent_package: None,
        handoff_targets: HashMap::new(),
        emit_tool_call_fn: Arc::new(|_, _| {}),
        emit_tool_result_fn: Arc::new(|_, _| {}),
        emit_tool_blocked_fn: Arc::new(|_, _| {}),
        emit_tool_update_fn: Arc::new(|_, _| {}),
        confirm_tool_use_fn: Arc::new(|_, _, _| panic!("no approval expected")),
        prepare_handoff_fn: None,
    });

    let abort = create_abort_signal();
    let output = ok(run(&bash, "echo hi", &abort).await);
    assert_eq!(output.value["status"], "ok", "{}", output.value);
    assert_eq!(output.value["exit_code"], 0, "{}", output.value);
    assert!(
        output.value["trace"]
            .as_array()
            .is_none_or(|t| t.is_empty()),
        "pure shell dispatches no inner calls: {}",
        output.value
    );

    let error = recoverable(run(&bash, "fs_read --path a", &abort).await, "unknown");
    let partial = partial_result_of(&error).expect("failure carries partial result");
    assert_eq!(partial["exit_code"], 127, "unknown command: {}", partial);
    assert!(
        partial["trace"].as_array().is_none_or(|t| t.is_empty()),
        "denied names must never dispatch: {}",
        partial
    );

    // Releasing the strong owner dangles the weak slot: the next run fails
    // closed instead of dispatching through a dead context (#2630).
    drop(ctx);
    let error = recoverable(run(&bash, "echo hi", &abort).await, "dangling");
    let message = format!("{error:#}");
    assert!(
        message.contains("cannot run scripts in this execution context"),
        "released owner must fail closed: {message}"
    );
}

mod output;

fn faketool_declarations() -> Vec<harnx_core::tool::ToolDeclaration> {
    let mut declarations = inner_declarations();
    declarations.truncate(1);
    declarations[0].name = "faketool".to_string();
    declarations
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inner_calls_ask_for_structured_results() {
    let provider = Arc::new(StructuredProvider {
        origins: Mutex::new(Vec::new()),
    });
    let (bash, _slot, _ctx) = build_harness_with(
        provider.clone(),
        Vec::new(),
        None,
        true,
        (faketool_declarations(), Default::default()),
    );
    let abort = create_abort_signal();
    ok(run(&bash, "faketool --path a", &abort).await);
    assert_eq!(*provider.origins.lock().unwrap(), vec![true]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mapped_tool_prints_idiomatic_output() {
    let provider = Arc::new(StructuredProvider {
        origins: Mutex::new(Vec::new()),
    });
    let outputs = std::collections::BTreeMap::from([(
        "faketool".to_string(),
        crate::ScriptShape {
            output: Some(harnx_toolset::ScriptOutput {
                stdout: Some("text".into()),
                stderr: Some("notes".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
    )]);
    let (bash, _slot, _ctx) = build_harness_with(
        provider,
        Vec::new(),
        None,
        true,
        (faketool_declarations(), outputs),
    );
    let abort = create_abort_signal();
    let payload = ok(run(&bash, "faketool --path a | wc -l", &abort).await).value;
    assert_eq!(
        payload["stdout"].as_str().map(str::trim),
        Some("3"),
        "{payload}"
    );
    assert_eq!(payload["stderr"], "faketool: 2 more lines\n", "{payload}");
}

/// The two types are bridged by a serde round trip, which drops a field one
/// side lacks without a word. Both sides are written out in full so adding a
/// field to either breaks this test.
#[test]
fn script_output_converts_with_every_field() {
    let output = harnx_toolset::ScriptOutput {
        stdout: Some("out".into()),
        row: Some("{a}:{b}".into()),
        stderr_text: Some("err".into()),
        stderr: Some("notes".into()),
        stderr_after: Some("diff".into()),
        exit_code: Some("status".into()),
        error_exit_code: Some(7),
        summary: Some("like cat".into()),
    };
    let harnx_toolset::ScriptOutput {
        stdout,
        row,
        stderr_text,
        stderr,
        stderr_after,
        exit_code,
        error_exit_code,
        summary,
    } = output.clone();
    let expected = harnx_script::commands::OutputMapping {
        stdout,
        row,
        stderr_text,
        stderr,
        stderr_after,
        exit_code,
        error_exit_code,
        summary,
    };
    assert_eq!(super::output_mapping(&output), Some(expected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn help_lists_the_declared_result_fields() {
    let outputs = std::collections::BTreeMap::from([(
        "faketool".to_string(),
        crate::ScriptShape {
            output: None,
            output_schema: Some(json!({
                "type": "object",
                "properties": {"line_count": {"type": "integer", "description": "lines seen"}},
            })),
        },
    )]);
    let (bash, _slot, _ctx) = build_harness_with(
        FakeProvider::new(),
        Vec::new(),
        None,
        true,
        (faketool_declarations(), outputs),
    );
    let abort = create_abort_signal();
    let payload = ok(run(&bash, "help faketool", &abort).await).value;
    let stdout = payload["stdout"].as_str().unwrap_or_default();
    assert!(stdout.contains("FIELDS (-o json)"), "{payload}");
    assert!(stdout.contains("line_count"), "{payload}");
}

/// Counts and byte totals per `(name, status)` from the operator payload's
/// raw trace, the ground truth the rendered report is parsed back against.
fn trace_totals(payload: &Value) -> Vec<(String, String, u64, u64)> {
    let mut totals: Vec<(String, String, u64, u64)> = Vec::new();
    for entry in payload["trace"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let status = entry["status"].as_str().unwrap();
        let bytes = entry["bytes"].as_u64().unwrap();
        match totals.iter_mut().find(|t| t.0 == name && t.1 == status) {
            Some(t) => {
                t.2 += 1;
                t.3 += bytes;
            }
            None => totals.push((name.into(), status.into(), 1, bytes)),
        }
    }
    totals.sort();
    totals
}

fn parsed_totals(payload: &Value) -> Vec<(String, String, u64, u64)> {
    let text = payload["content"][0]["text"].as_str().unwrap();
    let parsed = crate::script_usage::parse_report(text);
    assert_eq!(parsed.unparsed_lines, 0, "{text}");
    let mut totals: Vec<(String, String, u64, u64)> = Vec::new();
    for r in &parsed.rows {
        match totals.iter_mut().find(|t| t.0 == r.name && t.1 == r.status) {
            Some(t) => {
                t.2 += r.count;
                t.3 += r.bytes;
            }
            None => totals.push((r.name.clone(), r.status.clone(), r.count, r.bytes)),
        }
    }
    totals.sort();
    totals
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn script_usage_parser_reads_the_per_call_report() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let script =
        "set +e\nfs_read --path a >/dev/null\nfs_read --path b >/dev/null\nbroken --path x\nfs_read --path c >/dev/null\n";
    let payload = ok(run(&bash, script, &abort).await).value;
    let expected = trace_totals(&payload);
    assert_eq!(expected.iter().map(|t| t.2).sum::<u64>(), 4);
    assert!(expected.iter().any(|t| t.1 == "tool_error"), "{expected:?}");
    assert_eq!(parsed_totals(&payload), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn script_usage_parser_reads_the_grouped_report() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let abort = create_abort_signal();
    let script = format!(
        "set +e\n{}{}{}",
        "fs_read --path a >/dev/null\n".repeat(8),
        "broken --path x\n".repeat(3),
        "fs_read --path a >/dev/null\n"
    );
    let payload = ok(run(&bash, &script, &abort).await).value;
    let expected = trace_totals(&payload);
    assert_eq!(expected.iter().map(|t| t.2).sum::<u64>(), 12);
    assert_eq!(parsed_totals(&payload), expected);
}
