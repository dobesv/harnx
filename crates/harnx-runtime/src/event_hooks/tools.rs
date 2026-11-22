use super::operations;
use crate::{
    config::{Config, ResolvedEventHook},
    middleware::tools::{tool_commands, ScriptTools},
    utils::AbortSignal,
};
use anyhow::{ensure, Result};
use harnx_core::hooks::HookEventKind;
use harnx_engine::tool::ToolEvalContext;
use harnx_script::ScriptCommand;
use std::sync::Arc;

pub(super) struct EventTools {
    pub config: Config,
    pub evaluation: Arc<ToolEvalContext>,
    pub abort: AbortSignal,
}

#[derive(Default)]
pub(super) struct HookTools {
    pub commands: Vec<Arc<dyn ScriptCommand>>,
    pub executor: Option<Arc<ScriptTools>>,
}

impl EventTools {
    pub fn for_hook(&self, hook: &ResolvedEventHook, event: HookEventKind) -> Result<HookTools> {
        let allowed_names = hook.allowed_tool_names(
            &self.config,
            self.evaluation.current_agent_package.as_deref(),
        );
        // Middleware rejects collisions rather than letting a tool replace an
        // operation. Keep the same rule for lifecycle operations.
        ensure!(
            !allowed_names
                .iter()
                .any(|name| operations::names(event).contains(&name.as_str())),
            "allowlisted tool collides with an event-hook operation"
        );
        let declarations = self
            .evaluation
            .render
            .as_ref()
            .map(|render| {
                render
                    .decl_map
                    .values()
                    .filter(|tool| allowed_names.contains(&tool.name))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let executor = Arc::new(ScriptTools::event_hook(
            self.evaluation.clone(),
            self.abort.clone(),
            allowed_names,
        ));
        Ok(HookTools {
            commands: tool_commands(&declarations, executor.clone())?,
            executor: Some(executor),
        })
    }
}
