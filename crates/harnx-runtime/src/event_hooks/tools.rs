use crate::{
    config::{Config, ResolvedEventHook},
    event_hook_operations::names,
    script_tools::{tool_commands, ScriptTools},
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

pub(super) enum ToolContext {
    Ready(Box<EventTools>),
    TimedOut(String),
    Unused,
}

impl ToolContext {
    pub async fn prepare<F>(
        config: &crate::config::GlobalConfig,
        hooks: &[ResolvedEventHook],
        abort: &AbortSignal,
        tool_context: impl FnOnce() -> F,
    ) -> Self
    where
        F: std::future::Future<Output = ToolEvalContext>,
    {
        let timeout = hooks
            .iter()
            .filter(|hook| !hook.tools.is_empty())
            .map(|hook| hook.timeout)
            .max();
        let Some(timeout) = timeout else {
            return Self::Unused;
        };
        let evaluation = tokio::select! {
            biased;
            () = harnx_core::abort::wait_abort_signal(abort) => return Self::Unused,
            () = tokio::time::sleep(timeout) => {
                return Self::TimedOut(format!("tool context setup timed out after {timeout:?}"));
            }
            // Defer factory invocation until polling, so a pre-aborted turn skips it.
            evaluation = async { tool_context().await } => evaluation,
        };
        Self::Ready(Box::new(EventTools {
            evaluation: Arc::new(evaluation),
            config: config.read().clone(),
            abort: abort.clone(),
        }))
    }

    pub fn for_hook(
        &self,
        hook: &ResolvedEventHook,
        event: HookEventKind,
    ) -> std::result::Result<HookTools, String> {
        if hook.tools.is_empty() {
            return Ok(HookTools::default());
        }
        match self {
            Self::Ready(context) => context
                .for_hook(hook, event)
                .map_err(|error| format!("{error:#}")),
            Self::TimedOut(failure) => Err(failure.clone()),
            // Tool-declaring hooks never reach this arm; the early return handles tool-free hooks.
            Self::Unused => Ok(HookTools::default()),
        }
    }
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
                .any(|name| names(event).contains(&name.as_str())),
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
