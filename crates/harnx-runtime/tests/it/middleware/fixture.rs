use anyhow::Result;
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    execution_context::ExecutionContextObservation,
    instance::ServerScope,
    tool::{ToolCall, ToolError, ToolProvider, ToolProviderOutput, ToolResult},
};
use harnx_engine::tool::{eval_tool_calls, ToolEvalContext};
use harnx_runtime::{
    bash_provider::BashEvalContextSlot,
    config::{agent::Agent, AgentConfig, Config, ConfigLock, GlobalConfig},
    tool::{build_tool_eval_context, BuildToolEvalContextParams},
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

pub(super) fn bash(source: &str) -> Value {
    json!({"name":"test", "match":"^probe_run$", "bash":source})
}

pub(super) fn python(source: &str) -> Value {
    json!({"name":"test", "match":"^probe_run$", "python":source})
}

pub(super) fn call(arguments: Value) -> ToolCall {
    ToolCall::new("probe_run".into(), arguments, Some("outer".into()), None)
}

#[derive(Default)]
pub(super) struct Probe {
    pub calls: Mutex<Vec<(String, Value)>>,
    pub started: tokio::sync::Notify,
    pub dropped: tokio::sync::Notify,
}

struct Active<'a>(&'a Probe);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.dropped.notify_one();
    }
}

impl Probe {
    pub async fn invoke(
        &self,
        name: &str,
        args: Value,
    ) -> std::result::Result<ToolProviderOutput, ToolError> {
        self.calls.lock().push((name.to_string(), args.clone()));
        self.started.notify_one();
        let _active = Active(self);
        if args["hang"] == true {
            std::future::pending::<()>().await;
        }
        if let Some(ms) = args["delay_ms"].as_u64() {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        if args["fatal"] == true {
            return Err(ToolError::Fatal(
                std::io::Error::from(std::io::ErrorKind::PermissionDenied).into(),
            ));
        }
        if args["recoverable"] == true {
            return Err(ToolError::Recoverable(anyhow::anyhow!(
                "recoverable probe failure"
            )));
        }
        let mut output = ToolProviderOutput::new(args.clone());
        if args["media"] == true {
            let data = if args["value"] == "second" {
                "c2Vjb25k"
            } else {
                "Zmlyc3Q="
            };
            output.value = json!({"content":[{"type":"text","text":args["value"].to_string()},
                {"type":"image","data":data,"mimeType":"image/png"}]});
            let path = format!("/workspace/{}", args["value"].as_str().unwrap());
            output.execution_context = Some(ExecutionContextObservation::observe(
                std::path::Path::new(&path),
                std::path::Path::new(&path),
            ));
        }
        Ok(output)
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
        self.invoke(name, args).await
    }
}

pub(super) struct Fixture {
    pub ctx: ToolEvalContext,
    pub probe: Arc<Probe>,
    _owner: Arc<ToolEvalContext>,
}

pub(super) fn config(entries: Vec<Value>) -> GlobalConfig {
    let mut config = Config {
        data: harnx_core::config_data::ConfigData {
            middleware: serde_json::from_value(json!(entries)).unwrap(),
            ..Default::default()
        },
        ..Config::default()
    };
    let mut agent = AgentConfig::from_prompt("Middleware test");
    agent.set_use_tools(Some(vec!["probe_*".into(), "bash".into()]));
    agent.set_bash_tools(Some(vec!["probe_*".into()]));
    config.agent = Some(Agent::new(agent));
    *config.nats_tool_declarations.write() = ["probe_run", "probe_aux"]
        .into_iter()
        .map(|name| {
            serde_json::from_value(
                json!({"name":name,"description":"Probe", "parameters":{"type":"object"}}),
            )
            .unwrap()
        })
        .collect();
    Arc::new(ConfigLock::new(config))
}

impl Fixture {
    pub async fn new(entries: Vec<Value>) -> Self {
        let config = config(entries);
        let slot = Arc::new(BashEvalContextSlot::default());
        let scope = ServerScope::new();
        let mut params = BuildToolEvalContextParams::new(&config, &scope);
        params.bash_context_slot = Some(slot.clone());
        let mut ctx = build_tool_eval_context(params).await;
        let probe = Arc::new(Probe::default());
        ctx.providers.push(probe.clone());
        let ctx = Arc::new(ctx);
        slot.set(&ctx);
        // BashProvider upgrades the slot only for the duration of a call.
        // Keep its context alive through the provider's strong fixture guard.
        Self {
            ctx: (*ctx).clone(),
            probe,
            _owner: ctx,
        }
    }

    pub fn calls(&self) -> Vec<(String, Value)> {
        self.probe.calls.lock().clone()
    }

    pub async fn eval(&self, arguments: Value) -> Result<ToolResult> {
        Ok(
            eval_tool_calls(&self.ctx, vec![call(arguments)], &create_abort_signal())
                .await?
                .remove(0),
        )
    }
}
