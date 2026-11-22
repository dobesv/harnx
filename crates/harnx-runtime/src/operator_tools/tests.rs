use super::*;
use harnx_core::{
    abort::create_abort_signal,
    instance::ServerScope,
    tool::{ToolError, ToolProvider, ToolProviderOutput, ToolResult},
};
use harnx_engine::tool::{
    InnerToolCallStatus, MaterializedToolResult, ToolEvalRenderContext, ToolMiddlewareOutcome,
    ToolUseConfirmation,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

struct Provider {
    seen: Arc<Mutex<Vec<Value>>>,
    output: Value,
    wait: bool,
}
#[async_trait::async_trait]
impl ToolProvider for Provider {
    fn name(&self) -> &str {
        "operator-fixture"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "visible" || name == "hidden"
    }
    async fn call_tool(
        &self,
        _: &str,
        args: Value,
        abort: &crate::utils::AbortSignal,
    ) -> std::result::Result<ToolProviderOutput, ToolError> {
        if self.wait {
            crate::utils::wait_abort_signal(abort).await;
            return Err(ToolError::Fatal(anyhow::anyhow!("cancelled fixture")));
        }
        if !args.get("text").is_some_and(Value::is_string) {
            return Err(ToolError::Recoverable(anyhow::anyhow!(
                "text must be a string"
            )));
        }
        self.seen.lock().unwrap().push(args);
        Ok(self.output.clone().into())
    }
}

fn context(provider: Arc<dyn ToolProvider>) -> ToolEvalContext {
    let scope = ServerScope::new();
    let decls: HashMap<_, _> = ["visible", "hidden"]
        .into_iter()
        .map(|name| {
            let mut declaration: ToolDeclaration = serde_json::from_value(json!({
                "name": name,
                "description": name,
                "parameters": {"type":"object", "properties":{"text":{"type":"string"}}, "required":["text"]}
            }))
            .unwrap();
            declaration.mcp_server_name = Some("fixture".into());
            (name.into(), declaration)
        })
        .collect();
    ToolEvalContext {
        on_tool_call_completed: None,
        on_interaction: None,
        middleware_fn: None,
        work_boundary: None,
        instance_id: scope,
        tool_round: None,
        render: Some(ToolEvalRenderContext {
            decl_map: Arc::new(decls),
        }),
        providers: vec![provider],
        allowed_tool_names: HashSet::from(["visible".into()]),
        current_agent_package: None,
        handoff_targets: HashMap::new(),
        emit_tool_call_fn: Arc::new(|_, _| {}),
        emit_tool_result_fn: Arc::new(|_, _| {}),
        emit_tool_blocked_fn: Arc::new(|_, _| {}),
        emit_tool_update_fn: Arc::new(|_, _| {}),
        confirm_tool_use_fn: Arc::new(|_, _, _| ToolUseConfirmation::Defer),
        prepare_handoff_fn: None,
    }
}

fn fixture(output: Value) -> (ToolEvalContext, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    (
        context(Arc::new(Provider {
            seen: seen.clone(),
            output,
            wait: false,
        })),
        seen,
    )
}

fn call(raw: &str) -> OperatorToolCommand {
    OperatorToolCommand::Call {
        name: "visible".into(),
        args_json: raw.into(),
        tool_input: None,
    }
}

#[test]
fn parser_preserves_raw_json_remainder_and_action_subject_grammar() {
    let raw = "{\n  \"text\": \"a \\\"quoted\\\" path with spaces\", \"nested\": {\"array\": [1, null]}\n}  ";
    assert_eq!(
        parse_operator_line(&format!(" .call   tool\tvisible   {raw}")).unwrap(),
        call(raw)
    );
    assert_eq!(
        parse_operator_line(".list tools").unwrap(),
        OperatorToolCommand::List { pattern: None }
    );
    assert_eq!(
        parse_operator_line(".list tools visible*").unwrap(),
        OperatorToolCommand::List {
            pattern: Some("visible*".into())
        }
    );
    assert_eq!(
        parse_operator_line(".info tool visible").unwrap(),
        OperatorToolCommand::Info {
            name: "visible".into()
        }
    );
    // `.info tools` routes to the same worker listing as `.list tools`
    // (#2628): the session's real tools only exist on the worker.
    assert_eq!(
        parse_operator_line(".info tools").unwrap(),
        OperatorToolCommand::List { pattern: None }
    );
    assert!(is_operator_tool_command(".info tools"));
    assert!(is_operator_tool_command(".list tools"));
    assert!(!is_operator_tool_command(".info toolsy"));
    for line in [
        ".info tools visible",
        ".list tool",
        ".call tools visible {}",
        ".call tool visible",
        ".call tool visible null",
        ".call tool visible []",
        ".call tool visible 1",
        ".call tool visible {broken}",
        ".info tool visible extra",
        ".list tools * extra",
    ] {
        assert!(parse_operator_line(line).is_err(), "{line}");
    }
}

#[tokio::test]
async fn list_info_call_and_patterns_never_expose_or_dispatch_hidden_tools() {
    for command in [
        OperatorToolCommand::Info {
            name: "hidden".into(),
        },
        OperatorToolCommand::Call {
            name: "hidden".into(),
            args_json: "{}".into(),
            tool_input: None,
        },
    ] {
        let (ctx, seen) = fixture(json!(null));
        // Hidden tools are denied as a typed unauthorized-tool reply,
        // not a transport error: operators see a
        // structured denial instead of a string-matched failure.
        let reply = evaluate(
            ctx,
            &command,
            ToolOutputFormat::Human,
            &create_abort_signal(),
        )
        .await
        .unwrap();
        assert!(reply
            .error
            .as_deref()
            .unwrap_or("")
            .contains("not available"));
        assert_eq!(
            reply.error_kind,
            Some(super::OperatorToolErrorKind::UnauthorizedTool)
        );
        assert!(seen.lock().unwrap().is_empty());
    }
    for (pattern, count) in [
        (None, 1),
        (Some("*".into()), 1),
        (Some("hidden*".into()), 0),
        (Some("vis?ble".into()), 1),
    ] {
        let (ctx, seen) = fixture(json!(null));
        let reply = evaluate(
            ctx,
            &OperatorToolCommand::List { pattern },
            ToolOutputFormat::Json,
            &create_abort_signal(),
        )
        .await
        .unwrap();
        let tools: Vec<Value> = serde_json::from_str(&reply.output).unwrap();
        assert_eq!(tools.len(), count);
        assert!(tools.iter().all(|tool| tool["name"] == "visible"));
        assert!(seen.lock().unwrap().is_empty());
    }
    let (ctx, _) = fixture(json!(null));
    let reply = evaluate(
        ctx,
        &OperatorToolCommand::Info {
            name: "visible".into(),
        },
        ToolOutputFormat::Human,
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(reply.output.contains("Input schema:") && reply.output.contains("fixture"));
}

#[tokio::test]
async fn deny_and_validation_failures_remain_errors_with_full_output() {
    let (mut ctx, seen) = fixture(json!(null));
    ctx.middleware_fn = Some(Arc::new(|prepared, _, _| {
        let call = prepared.call.clone();
        Box::pin(async move {
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(
                        call,
                        json!({"error":"policy deny", "is_error":true, "blocked_by_middleware":"policy"}),
                    ),
                    InnerToolCallStatus::Blocked,
                ),
            ))
        })
    }));
    let reply = evaluate(
        ctx,
        &call(r#"{"text":"ok"}"#),
        ToolOutputFormat::Json,
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert_eq!(reply.error.as_deref(), Some("policy deny"));
    assert_eq!(
        serde_json::from_str::<Value>(&reply.output).unwrap(),
        json!({"error":"policy deny", "is_error":true, "blocked_by_middleware":"policy"})
    );
    assert!(seen.lock().unwrap().is_empty());
    let (ctx, seen) = fixture(json!(null));
    let reply = evaluate(
        ctx,
        &call(r#"{"text":123}"#),
        ToolOutputFormat::Json,
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(reply.error.is_some(), "{reply:?}");
    assert!(reply.output.contains("text must be a string"));
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mixed_structured_error_partial_and_null_results_are_not_reduced() {
    for output in [
        json!(null),
        json!({"content":[{"type":"text","text":"quoted \"name\""}, {"type":"image","data":"aW1hZ2U=","mimeType":"image/png"}],
        "structuredContent":{"items":[1,null]}, "_meta":{"keep":true}}),
        json!({"isError":true,"content":[{"type":"text","text":"failed"}]}),
        json!({"resultType":"partial","content":[{"type":"text","text":"unfinished"}]}),
    ] {
        let (ctx, _) = fixture(output.clone());
        let reply = evaluate(
            ctx,
            &call(r#"{"text":"ok"}"#),
            ToolOutputFormat::Json,
            &create_abort_signal(),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reply.output).unwrap(),
            output
        );
        assert_eq!(reply.error.is_some(), result_error(&output).is_some());
    }
}

#[tokio::test]
async fn cancellation_during_provider_and_before_dispatch_is_not_success() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ctx = context(Arc::new(Provider {
        seen: seen.clone(),
        output: json!(true),
        wait: true,
    }));
    let abort = create_abort_signal();
    let signal = abort.clone();
    let cancel = tokio::spawn(async move {
        tokio::task::yield_now().await;
        signal.set_ctrlc();
    });
    assert!(evaluate(
        ctx,
        &call(r#"{"text":"ok"}"#),
        ToolOutputFormat::Json,
        &abort
    )
    .await
    .is_err());
    cancel.await.unwrap();
    assert!(seen.lock().unwrap().is_empty());
    let (ctx, seen) = fixture(json!(null));
    assert!(evaluate(ctx, &call("{}"), ToolOutputFormat::Human, &abort)
        .await
        .is_err());
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn handlers_reject_missing_session_and_invalid_json_before_discovery() {
    let config = Arc::new(crate::config::ConfigLock::new(
        crate::config::Config::default(),
    ));
    for line in [
        ".info tool visible",
        ".info tools",
        ".list tools",
        r#".call tool visible {"text":"a b"}"#,
    ] {
        let mut output = Vec::new();
        let error = crate::commands::run_command_with_output(
            &config,
            create_abort_signal(),
            line,
            &mut output,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "Tool commands require an active session");
        assert!(output.is_empty());
    }
    let mut output = Vec::new();
    let error = crate::commands::run_command_with_output(
        &config,
        create_abort_signal(),
        ".call tool visible []",
        &mut output,
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "Tool arguments must be a JSON object");
}

mod bash;
mod protocol;
mod selection;

#[tokio::test]
async fn operator_call_tool_input_reaches_the_leaf_dispatch_scope() {
    // Decision 11: `evaluate` scopes a Call's `tool_input` to its leaf
    // dispatch. The NATS provider builds `ToolRequest.input_responses` /
    // `request_state` from `middleware::current_tool_input()`, so this
    // fixture reads that scope: present exactly for a call that carries
    // `tool_input`, absent for one that doesn't.
    struct ScopedProvider(Arc<Mutex<Vec<Option<crate::middleware::ToolInputResume>>>>);
    #[async_trait::async_trait]
    impl ToolProvider for ScopedProvider {
        fn name(&self) -> &str {
            "operator-scoped-fixture"
        }
        fn has_tool(&self, name: &str) -> bool {
            name == "visible"
        }
        async fn call_tool(
            &self,
            _: &str,
            _args: Value,
            _: &crate::utils::AbortSignal,
        ) -> std::result::Result<ToolProviderOutput, ToolError> {
            self.0
                .lock()
                .unwrap()
                .push(crate::middleware::current_tool_input());
            Ok(Value::Null.into())
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ctx = context(Arc::new(ScopedProvider(seen.clone())));
    let abort = create_abort_signal();
    fn scoped_call(args_json: &str, tool_input: Option<OperatorToolInput>) -> OperatorToolCommand {
        OperatorToolCommand::Call {
            name: "visible".into(),
            args_json: args_json.into(),
            tool_input,
        }
    }
    evaluate(
        ctx,
        &scoped_call(
            r#"{"text":"a"}"#,
            Some(OperatorToolInput::url_response(
                "connect-round-1".into(),
                harnx_core::tool::ToolInputRequired::URL_ACTION_CONTINUE,
                json!({ "challenge_id": "abc", "round": 1 }),
            )),
        ),
        crate::tool_output::ToolOutputFormat::Json,
        &abort,
    )
    .await
    .unwrap();
    evaluate(
        context(Arc::new(ScopedProvider(seen.clone()))),
        &scoped_call(r#"{"text":"b"}"#, None),
        crate::tool_output::ToolOutputFormat::Json,
        &abort,
    )
    .await
    .unwrap();
    let seen = seen.lock().unwrap();
    let Some(input) = seen[0].as_ref() else {
        panic!("tool_input must reach the dispatch scope")
    };
    assert_eq!(
        input.responses.get("connect-round-1").unwrap().action,
        harnx_core::tool::ToolInputRequired::URL_ACTION_CONTINUE
    );
    assert_eq!(input.request_state["challenge_id"], json!("abc"));
    assert!(
        seen[1].is_none(),
        "a call without tool_input must not see another call's answers"
    );
}

/// The tri-state `OperatorToolRequest::user_id` serde contract: the field
/// serializes as a plain optional, and an explicit `null` deserializes as
/// `Some(None)` (ownerless operator) rather than a legacy absent field.
#[test]
fn operator_user_id_tri_state_serializes_and_deserializes() {
    fn serialized(value: &Option<Option<String>>) -> Value {
        let mut buf = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut buf);
        super::operator_user_id::serialize(value, &mut serializer).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    fn deserialized(raw: &str) -> Option<Option<String>> {
        let mut deserializer = serde_json::Deserializer::from_str(raw);
        super::operator_user_id::deserialize(&mut deserializer).unwrap()
    }

    assert_eq!(serialized(&None), Value::Null);
    assert_eq!(serialized(&Some(None)), Value::Null);
    assert_eq!(serialized(&Some(Some("alice".into()))), json!("alice"));
    assert_eq!(deserialized("null"), Some(None));
    assert_eq!(deserialized("\"alice\""), Some(Some("alice".into())));
}
