//! Load shipped agents, not parallel copies of their confirmation scripts.
use super::{documentation::normalize_line_endings, fixture::Probe};
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    config_data::ConfigData,
    instance::ServerScope,
    interaction::InteractionResponse,
    tool::{ToolCall, ToolError, ToolProvider, ToolProviderOutput},
};
use harnx_engine::tool::{eval_tool_round, PreparedHandoff, ToolEvalContext, ToolRoundOutcome};
use harnx_runtime::{
    config::{agent, Config, ConfigLock},
    middleware::{with_resume, AcceptedInteraction, MiddlewareResume},
    tool::{build_tool_eval_context, BuildToolEvalContextParams},
};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct TimeProbe(Arc<Probe>);
#[async_trait::async_trait]
impl ToolProvider for TimeProbe {
    fn name(&self) -> &str {
        "shipped-demo-time"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "time_get_current_time"
    }
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.0.invoke(name, args).await
    }
}

async fn context(path: &str, tool: &str) -> (ToolEvalContext, Arc<Probe>) {
    let loaded = agent::load(&root().join(path)).unwrap();
    assert!(
        loaded.hooks().is_none(),
        "shipped confirm isn't legacy HITL"
    );
    assert!(loaded.use_tools().unwrap().iter().any(|name| name == tool));
    let config = Config {
        agent: Some(loaded),
        ..Config::default()
    };
    let chain = config.resolved_middleware().unwrap();
    assert_eq!(chain.entries.len(), 1);
    assert!(chain.entries[0].matcher.is_match(tool).unwrap());
    assert!(!chain.entries[0].matcher.is_match("unrelated_tool").unwrap());
    let global = Arc::new(ConfigLock::new(config));
    let scope = ServerScope::new();
    let mut ctx = build_tool_eval_context(BuildToolEvalContextParams::new(&global, &scope)).await;
    ctx.allowed_tool_names.insert(tool.into());
    ctx.on_interaction = Some(Arc::new(|_| Box::pin(async { Ok(()) })));
    let probe = Arc::new(Probe::default());
    ctx.providers.push(Arc::new(TimeProbe(probe.clone())));
    let handoffs = probe.clone();
    ctx.prepare_handoff_fn = Some(Arc::new(move |agent, session_id, call_id| {
        handoffs
            .calls
            .lock()
            .push((agent.clone(), json!({"id":call_id})));
        Box::pin(async move {
            Ok(PreparedHandoff {
                agent,
                session_id: session_id.unwrap(),
            })
        })
    }));
    (ctx, probe)
}

async fn decision(path: &str, tool: &str, action: &str, message: &str, denial: &str) {
    let (ctx, probe) = context(path, tool).await;
    let call = ToolCall::new(
        tool.into(),
        json!({"prompt":"execute plan", "session_id":"atlas-target", "timezone":"UTC"}),
        Some("shipped-call".into()),
        None,
    );
    let abort = create_abort_signal();
    let first = eval_tool_round(&ctx, vec![call.clone()], vec![], &abort)
        .await
        .unwrap();
    let ToolRoundOutcome::InputRequired(suspension) = first else {
        panic!("shipped configuration must suspend: {first:?}")
    };
    assert!(suspension.completed.is_empty());
    assert_eq!(suspension.pending.len(), 1);
    let pending = &suspension.pending[0];
    assert_eq!(pending.interaction.kind, "confirm");
    assert_eq!(pending.interaction.payload["harnx"]["kind"], "confirm");
    assert_eq!(pending.interaction.summary, message);
    assert!(
        probe.calls.lock().is_empty(),
        "nothing dispatched before approval"
    );
    let resumed = with_resume(
        MiddlewareResume {
            tool_input: None,
            checkpoint: pending.checkpoint.clone(),
            responses: vec![AcceptedInteraction {
                request: pending.interaction.clone(),
                response: InteractionResponse {
                    action: action.into(),
                    data: Default::default(),
                    note: Some("operator audit".into()),
                },
            }],
        },
        eval_tool_round(&ctx, vec![call], vec![], &abort),
    )
    .await
    .unwrap();
    let ToolRoundOutcome::Completed(results) = resumed else {
        panic!("accepted answer must finish shipped call: {resumed:?}")
    };
    assert_eq!(results.len(), 1);
    if action == "approve" {
        assert_eq!(probe.calls.lock().len(), 1);
        if tool == "atlas_session_handoff" {
            assert_eq!(results[0].switch_agent.as_ref().unwrap().agent, "atlas");
        } else {
            assert_eq!(results[0].output["timezone"], "UTC");
        }
    } else {
        assert!(probe.calls.lock().is_empty());
        assert_eq!(results[0].output["is_error"], true);
        assert_eq!(results[0].output["error"], denial);
        assert!(results[0].switch_agent.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_shipped_daedalus_handoff_approve_and_deny() {
    for action in ["approve", "deny"] {
        decision(
            "packages/pantheon/agents/daedalus.md",
            "atlas_session_handoff",
            action,
            "Hand off this plan to Atlas for execution?",
            "Atlas handoff denied by the user.",
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_shipped_demo_time_approve_and_deny() {
    for action in ["approve", "deny"] {
        decision(
            "demos/config/agents/tool-confirm-agent.md",
            "time_get_current_time",
            action,
            "Manual approval required",
            "Time lookup denied by the user.",
        )
        .await;
    }
}

// Agent front-matter extraction strips the final newline when middleware is last.
fn normalized_middleware(middleware: &impl serde::Serialize) -> serde_json::Value {
    let mut value = serde_json::to_value(middleware).unwrap();
    for entry in value.as_array_mut().unwrap() {
        if let Some(script) = entry.get_mut("bash") {
            *script = script.as_str().unwrap().trim_end().into();
        }
    }
    value
}

fn documented_middleware(path: &str) -> Vec<serde_json::Value> {
    let guide = std::fs::read_to_string(root().join(path)).unwrap();
    normalize_line_endings(&guide)
        .split("```yaml\n")
        .skip(1)
        .filter_map(|block| {
            let yaml = block.split("```").next().unwrap();
            if !yaml.starts_with("middleware:\n") {
                return None;
            }
            let config: ConfigData = serde_yaml::from_str(yaml).unwrap();
            Some(normalized_middleware(&config.middleware))
        })
        .collect()
}

#[test]
fn middleware_shipped_documentation_matches_executed_agents() {
    let agents = [
        "demos/config/agents/tool-confirm-agent.md",
        "packages/pantheon/agents/daedalus.md",
    ];
    let shipped: Vec<_> = agents
        .into_iter()
        .map(|path| {
            let loaded = agent::load(&root().join(path)).unwrap();
            normalized_middleware(&loaded.middleware)
        })
        .collect();
    assert_eq!(
        documented_middleware("docs/tool-confirmation-guide.md"),
        shipped
    );
    assert_eq!(
        documented_middleware("docs/agent-guide.md"),
        vec![shipped[1].clone()]
    );
}
