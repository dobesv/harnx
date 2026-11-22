use super::super::{config_with_hooks, hook, python_hook};
use harnx_core::{
    abort::AbortSignal,
    hooks::{EventHookEntry, EventHookOnError, HookEventKind},
    instance::ServerScope,
    tool::{ToolCallOrigin, ToolError, ToolProgress, ToolProvider, ToolProviderOutput},
};
use harnx_engine::tool::ToolEvalContext;
use harnx_runtime::{
    config::GlobalConfig,
    event_hooks::{run_event_hooks, EventHookInput, EventHookOutcome},
    tool::{build_tool_eval_context, BuildToolEvalContextParams},
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Notify;

pub(super) fn tool_hook(python: bool, source: &str, on_error: EventHookOnError) -> EventHookEntry {
    let mut entry = if python {
        python_hook("helper", source)
    } else {
        hook("helper", source, on_error)
    };
    if let EventHookEntry::Full(definition) = &mut entry {
        definition.tools = vec!["hook_probe".into()];
        definition.on_error = on_error;
    }
    entry
}

#[derive(Debug, Clone)]
pub(super) struct Call {
    pub name: String,
    pub arguments: Value,
    pub id: String,
    pub round: Option<u64>,
}

#[derive(Default)]
pub(super) struct Probe {
    pub calls: Mutex<Vec<Call>>,
    pub started: Notify,
    pub cancelled: AtomicUsize,
}

struct Pending<'a>(&'a Probe);
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ToolProvider for Probe {
    fn name(&self) -> &str {
        "hook-probe"
    }

    fn has_tool(&self, name: &str) -> bool {
        matches!(name, "hook_probe" | "hook_forbidden" | "add_context")
    }

    async fn call_tool(
        &self,
        _name: &str,
        _arguments: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        unreachable!("engine must use progress-aware dispatch")
    }

    async fn call_tool_with_progress(
        &self,
        name: &str,
        arguments: Value,
        origin: ToolCallOrigin<'_>,
        _abort: &AbortSignal,
        _progress: Arc<dyn ToolProgress>,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.calls.lock().push(Call {
            name: name.into(),
            arguments: arguments.clone(),
            id: origin.tool_call_id.expect("fresh hook call id").into(),
            round: origin.tool_round,
        });
        self.started.notify_one();
        if arguments["hang"] == true {
            let _pending = Pending(self);
            std::future::pending::<()>().await;
        }
        if arguments["recoverable"] == true {
            return Err(ToolError::Recoverable(anyhow::anyhow!(
                "hook provider failure"
            )));
        }
        if arguments["fatal"] == true {
            return Err(ToolError::Fatal(anyhow::anyhow!(
                "fatal hook provider failure"
            )));
        }
        if arguments["interact"] == true {
            return Ok(ToolProviderOutput {
                value: Value::Null,
                execution_context: None,
                input_required: Some(harnx_core::tool::ToolInputRequired {
                    producer: "tool:hook-probe/hook_probe".into(),
                    key: "confirm-hook".into(),
                    kind: "confirm".into(),
                    message: Some("Approve hook?".into()),
                    surface: None,
                    summary: "Approve hook?".into(),
                    request_state: json!({}),
                }),
            });
        }
        Ok(ToolProviderOutput::new(json!({
            "content": [{"type": "text", "text": arguments["message_value"].as_str().unwrap_or("probe output")}]
        })))
    }
}

pub(super) struct Fixture {
    pub config: GlobalConfig,
    pub context: ToolEvalContext,
    pub probe: Arc<Probe>,
}

impl Fixture {
    pub async fn new(entries: Vec<EventHookEntry>) -> Self {
        Self::with_middleware(entries, vec![]).await
    }

    pub async fn with_middleware(entries: Vec<EventHookEntry>, middleware: Vec<Value>) -> Self {
        let config = config_with_hooks(HookEventKind::SessionStart, entries);
        config.write().data.middleware = serde_json::from_value(json!(middleware)).unwrap();
        *config.read().nats_tool_declarations.write() =
            ["hook_probe", "hook_forbidden", "add_context"]
                .into_iter()
                .map(|name| {
                    serde_json::from_value(json!({
                        "name": name, "description": "Hook probe", "parameters": {
                            "type": "object", "properties": {
                                "message_value": {"type": "string"},
                                "recoverable": {"type": "boolean"},
                                "fatal": {"type": "boolean"},
                                "hang": {"type": "boolean"},
                                "interact": {"type": "boolean"}
                            }
                        }
                    }))
                    .unwrap()
                })
                .collect();
        let scope = ServerScope::new();
        let mut context = build_tool_eval_context(
            BuildToolEvalContextParams::new(&config, &scope).with_agent_use_tools(Some("*")),
        )
        .await;
        let probe = Arc::new(Probe::default());
        context.providers.push(probe.clone());
        // Hooks must detach even if an existing round context is reused.
        context.tool_round = Some(88);
        context.emit_tool_call_fn = Arc::new(|_, _| panic!("hook calls must not publish UI rows"));
        context.emit_tool_result_fn =
            Arc::new(|_, _| panic!("hook results must not publish UI rows"));
        Self {
            config,
            context,
            probe,
        }
    }

    pub async fn run(&self, abort: &AbortSignal) -> anyhow::Result<EventHookOutcome> {
        run_event_hooks(&self.config, &EventHookInput::default(), abort, || {
            std::future::ready(self.context.clone())
        })
        .await
    }
}
