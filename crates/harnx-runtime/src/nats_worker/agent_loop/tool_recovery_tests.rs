use super::*;
use harnx_core::tool::{
    ToolCall, ToolCallOrigin, ToolError, ToolProvider, ToolProviderOutput, ToolReplay,
};
use serde_json::json;

struct MixedRecovery;

#[async_trait::async_trait]
impl ToolProvider for MixedRecovery {
    fn name(&self) -> &str {
        "mixed-recovery"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "rerun"
    }
    async fn replay_tool_call(
        &self,
        replay: ToolReplay<'_>,
        _: &AbortSignal,
    ) -> Result<Option<ToolProviderOutput>, ToolError> {
        Ok((replay.call.name == "saved").then(|| ToolProviderOutput::new(json!("saved reply"))))
    }
    async fn call_tool(
        &self,
        _: &str,
        _: serde_json::Value,
        _: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        Ok(ToolProviderOutput::new(json!("rerun reply")))
    }
}

fn mixed_context(scope: harnx_core::instance::ServerScope) -> crate::tool::ToolEvalContext {
    crate::tool::ToolEvalContext {
        work_boundary: None,
        instance_id: scope,
        tool_round: None,
        render: None,
        providers: vec![Arc::new(MixedRecovery)],
        allowed_tool_names: Default::default(),
        current_agent_package: None,
        handoff_targets: Default::default(),
        emit_tool_call_fn: Arc::new(|_, _| {}),
        emit_tool_result_fn: Arc::new(|_, _| {}),
        emit_tool_blocked_fn: Arc::new(|_, _| {}),
        emit_tool_update_fn: Arc::new(|_, _| {}),
        confirm_tool_use_fn: Arc::new(|_, _, _| crate::tool::ToolUseConfirmation::Approve),
        dispatch_hook_fn: Arc::new(|_| {
            Box::pin(async {
                harnx_core::hooks::HookOutcome {
                    control: harnx_core::hooks::HookResultControl::Continue,
                    result: Default::default(),
                }
            })
        }),
    }
}

#[tokio::test]
async fn mixed_recovery_preserves_original_positions_including_anonymous_calls() -> Result<()> {
    let scope = harnx_core::instance::ServerScope::new();
    let config = Arc::new(crate::config::ConfigLock::new(
        crate::config::Config::default(),
    ));
    let mut repair = build_tool_repair_context(&config);
    let mut declaration: harnx_core::tool::ToolDeclaration = serde_json::from_value(json!({
        "name": "rerun", "description": "", "parameters": {"type": "object", "properties": {}}
    }))?;
    declaration.idempotent_hint = Some(true);
    repair.decl_map.insert("rerun".into(), declaration);
    let abort = crate::utils::create_abort_signal();
    let (_server, log) = recovery_fixture().await?;
    let args = RepairOrphanToolCallsArgs {
        config,
        instance_id: &scope,
        fence_token: None,
        worker_id: None,
        session_id: "parent",
        abort_signal: &abort,
        lease: None,
    };
    let calls = ["saved", "rerun", "unknown", "saved", "rerun"]
        .map(|name| ToolCall::new(name.into(), json!({}), None, None))
        .to_vec();
    let seq = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: calls.clone(),
            timestamp: None,
            fence_token: Some(1),
        })
        .await?;
    let orphan = PendingToolCalls {
        seq,
        text: String::new(),
        thought: None,
        calls,
        timestamp: None,
    };
    let mut eval = mixed_context(scope.clone());
    let results = repair_single_orphan(&orphan, &args, &repair, &mut eval).await?;
    assert_eq!(
        results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>(),
        ["saved", "rerun", "unknown", "saved", "rerun"]
    );
    assert_eq!(results[0].output, "saved reply");
    assert_eq!(results[1].output, "rerun reply");
    assert!(results[2].output["error"]
        .as_str()
        .unwrap()
        .contains("interrupted"));
    Ok(())
}

/// The call id and round of each rerun, in order.
type RecordedOrigins = Arc<std::sync::Mutex<Vec<(Option<String>, Option<u64>)>>>;

/// Records the transcript call and round each rerun was dispatched as.
struct RerunRecorder {
    origins: RecordedOrigins,
}

#[async_trait::async_trait]
impl ToolProvider for RerunRecorder {
    fn name(&self) -> &str {
        "rerun-recorder"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "rerun"
    }
    async fn call_tool(
        &self,
        _: &str,
        _: serde_json::Value,
        _: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        unreachable!("the engine dispatches through call_tool_with_progress")
    }
    async fn call_tool_with_progress(
        &self,
        _: &str,
        _: serde_json::Value,
        origin: ToolCallOrigin<'_>,
        _: &AbortSignal,
        _: Arc<dyn harnx_core::tool::ToolProgress>,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.origins
            .lock()
            .unwrap()
            .push((origin.tool_call_id.map(str::to_string), origin.tool_round));
        Ok(ToolProviderOutput::new(json!("rerun reply")))
    }
}

/// A rerun dispatches an orphan's call afresh, and it goes out under that
/// orphan's round: a replay after another restart looks for its journal row
/// there. One evaluation context serves every orphan a resume repairs.
#[tokio::test]
async fn each_orphans_reruns_are_dispatched_under_its_round() -> Result<()> {
    let scope = harnx_core::instance::ServerScope::new();
    let config = Arc::new(crate::config::ConfigLock::new(
        crate::config::Config::default(),
    ));
    let mut repair = build_tool_repair_context(&config);
    let mut declaration: harnx_core::tool::ToolDeclaration = serde_json::from_value(json!({
        "name": "rerun", "description": "", "parameters": {"type": "object", "properties": {}}
    }))?;
    declaration.idempotent_hint = Some(true);
    repair.decl_map.insert("rerun".into(), declaration);
    let abort = crate::utils::create_abort_signal();
    let (_server, log) = recovery_fixture().await?;
    let args = RepairOrphanToolCallsArgs {
        config,
        instance_id: &scope,
        fence_token: None,
        worker_id: None,
        session_id: "parent",
        abort_signal: &abort,
        lease: None,
    };
    let origins = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut eval = mixed_context(scope.clone());
    eval.providers = vec![Arc::new(RerunRecorder {
        origins: origins.clone(),
    })];

    let mut rounds = Vec::new();
    for call_id in ["first", "second"] {
        let calls = vec![ToolCall::new(
            "rerun".into(),
            json!({}),
            Some(call_id.into()),
            None,
        )];
        let seq = log
            .append_event_async(&SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: calls.clone(),
                timestamp: None,
                fence_token: Some(1),
            })
            .await?;
        rounds.push(seq);
        let orphan = PendingToolCalls {
            seq,
            text: String::new(),
            thought: None,
            calls,
            timestamp: None,
        };
        repair_single_orphan(&orphan, &args, &repair, &mut eval).await?;
    }

    assert_eq!(
        *origins.lock().unwrap(),
        [
            (Some("first".to_string()), Some(rounds[0])),
            (Some("second".to_string()), Some(rounds[1])),
        ]
    );
    Ok(())
}

async fn recovery_fixture() -> Result<(
    crate::nats_test_common::NatsServerHandle,
    crate::nats_session_log::NatsSessionLog,
)> {
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let log = crate::nats_session_log::NatsSessionLog::new_with_replicas(js, "parent", 1);
    Ok((server, log))
}
