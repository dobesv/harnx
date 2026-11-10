use super::*;

/// One installed command-scoped context, mirroring the production flow:
/// BashProvider first in dispatch order, inner declarations from one
/// admitted set, late allowed-name overwrite, then root-call approval.
struct BashHarness {
    owner: Arc<ToolEvalContext>,
    root_call_id: String,
}

impl BashHarness {
    fn new(seen: Arc<Mutex<Vec<Value>>>, admitted: &[&str]) -> Self {
        let slot = crate::bash_provider::BashEvalContextSlot::new();
        let inner = Arc::new(Provider {
            seen,
            output: json!({"content":[{"type":"text","text":"inner done"}]}),
            wait: false,
        });
        let inner_declarations = admitted_declarations(admitted);
        let bash = Arc::new(crate::bash_provider::BashProvider::new(
            crate::bash_provider::BashProviderConfig {
                active: true,
                inner_declarations: inner_declarations.clone(),
                base_providers: vec![inner.clone()],
                slot: Some(slot.clone()),
                payload_limit: None,
                operator_inner_consent: true,
            },
        ));
        let mut allowed: HashSet<String> = inner_declarations
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        allowed.insert(crate::config::BASH_TOOL_NAME.to_string());
        let scope = ServerScope::new();
        let mut ctx = ToolEvalContext {
            work_boundary: None,
            instance_id: scope,
            tool_round: Some(1),
            render: None,
            providers: vec![bash],
            allowed_tool_names: allowed,
            current_agent_package: None,
            handoff_targets: HashMap::new(),
            emit_tool_call_fn: Arc::new(|_, _| {}),
            emit_tool_result_fn: Arc::new(|_, _| {}),
            emit_tool_blocked_fn: Arc::new(|_, _| {}),
            emit_tool_update_fn: Arc::new(|_, _| {}),
            confirm_tool_use_fn: Arc::new(|_, _, _| ToolUseConfirmation::Defer),
            dispatch_hook_fn: Arc::new(|_| {
                Box::pin(async {
                    // Ask on every call, so admission depends on the context's
                    // confirmation callback: only the operator's root call may
                    // approve itself.
                    HookOutcome {
                        control: HookResultControl::Ask { reason: None },
                        result: HookResult::default(),
                    }
                })
            }),
            prepare_handoff_fn: None,
        };
        let root_call_id = uuid::Uuid::new_v4().to_string();
        approve_root_bash_call(&mut ctx, &root_call_id);
        let owner = slot.install(ctx);
        Self {
            owner,
            root_call_id,
        }
    }

    async fn run(
        &self,
        script: &str,
        timeout_secs: Option<u64>,
        format: ToolOutputFormat,
    ) -> Result<OperatorToolReply> {
        evaluate_bash(
            &self.owner,
            &self.root_call_id,
            BashInvocation {
                script,
                timeout_secs,
                format,
                abort: &create_abort_signal(),
            },
        )
        .await
    }
}

fn admitted_declarations(admitted: &[&str]) -> Vec<ToolDeclaration> {
    ["visible", "hidden"]
        .iter()
        .filter(|name| admitted.contains(name))
        .map(|name| ToolDeclaration {
            name: (*name).to_string(),
            description: String::new(),
            parameters: serde_json::from_value(
                json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
            )
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_bash_runs_admitted_tool_and_formats_full_result() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = BashHarness::new(seen.clone(), &["visible"]);
    let reply = harness
        .run("visible --text hi", None, ToolOutputFormat::Json)
        .await
        .unwrap();
    assert!(reply.error.is_none(), "{reply:?}");
    let payload: Value = serde_json::from_str(&reply.output).unwrap();
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["exit_code"], 0);
    assert_eq!(payload["trace"][0]["name"], "visible");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the admitted inner tool must dispatch exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_bash_pure_shell_works_with_empty_catalog() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = BashHarness::new(seen.clone(), &[]);
    let reply = harness
        .run("echo hello", None, ToolOutputFormat::Json)
        .await
        .unwrap();
    assert!(reply.error.is_none(), "{reply:?}");
    assert!(reply.output.contains("hello"), "{reply:?}");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_bash_unauthorized_tool_is_not_dispatched() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    // The provider knows `hidden`, but only `visible` is admitted.
    let harness = BashHarness::new(seen.clone(), &["visible"]);
    let reply = harness
        .run("hidden --text nope", None, ToolOutputFormat::Json)
        .await
        .unwrap();
    assert!(
        reply.error.is_some(),
        "unadmitted tool must fail: {reply:?}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_bash_nonzero_exit_reports_failure_with_exit_code() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = BashHarness::new(seen, &[]);
    let reply = harness
        .run("exit 3", None, ToolOutputFormat::Json)
        .await
        .unwrap();
    let error = reply.error.clone().expect("nonzero exit must fail");
    assert!(reply.output.contains("3"), "exit code missing: {reply:?}");
    assert!(
        error.contains("exit code 3") || reply.output.contains("\"exit_code\":3"),
        "exit code not carried in the failure: {reply:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_bash_positive_timeout_fails_cleanly() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = BashHarness::new(seen, &[]);
    let started = std::time::Instant::now();
    let reply = harness
        .run("sleep 30", Some(1), ToolOutputFormat::Json)
        .await
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "timeout must bound the script run"
    );
    assert!(
        reply.error.is_some(),
        "timed-out script must fail: {reply:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_bash_abort_before_run_reports_cancellation() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = BashHarness::new(seen, &[]);
    let abort = create_abort_signal();
    abort.set_ctrlc();
    let error = evaluate_bash(
        &harness.owner,
        &harness.root_call_id,
        BashInvocation {
            script: "echo hello",
            timeout_secs: None,
            format: ToolOutputFormat::Human,
            abort: &abort,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "Operator tool command cancelled");
}

/// Root Bash approval is keyed to both the tool name and the generated call
/// ID: the operator's own script runs, but no other bash call inherits it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_bash_root_approval_is_keyed_to_the_call_id() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = BashHarness::new(seen, &[]);
    // The approved root call runs the script.
    let reply = harness
        .run("echo approved", None, ToolOutputFormat::Json)
        .await
        .unwrap();
    assert!(reply.error.is_none(), "{reply:?}");
    // A bash call with any other ID stays deferred, even with the same name.
    let other = ToolCall {
        name: crate::config::BASH_TOOL_NAME.to_string(),
        arguments: json!({"script": "echo nope"}),
        id: Some(uuid::Uuid::new_v4().to_string()),
        thought_signature: None,
        reasoning_provenance: None,
    };
    let error = eval_inner_tool_call(&harness.owner, other, &create_abort_signal())
        .await
        .unwrap_err();
    assert!(
        error.downcast_ref::<ToolApprovalRequiredError>().is_some(),
        "unapproved bash call must defer: {error:#}"
    );
}
