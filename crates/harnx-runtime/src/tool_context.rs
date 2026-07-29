use crate::agent_loop::AgentLoopContext;
use crate::config::{Config, GlobalConfig, Input};
use crate::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use crate::tool::CompletionText;
use crate::utils::AbortSignal;
use harnx_core::instance::InstanceId;
use harnx_hooks::PersistentHookManager;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared runtime state for one tool-evaluation round.
pub struct ToolRoundParams<'a> {
    pub config: &'a GlobalConfig,
    pub instance_id: &'a InstanceId,
    pub input: &'a Input,
    pub completion: CompletionText<'a>,
    pub abort_signal: &'a AbortSignal,
    pub persistent_manager: &'a Arc<Mutex<PersistentHookManager>>,
    pub working_dir: Option<&'a Path>,
}

/// Inputs used to assemble the provider and rendering context for a tool round.
pub struct BuildToolEvalContextParams<'a> {
    pub config: &'a GlobalConfig,
    pub instance_id: &'a InstanceId,
    pub agent_use_tools: Option<&'a str>,
    pub current_agent_package: Option<String>,
    pub persistent_manager: &'a Arc<Mutex<PersistentHookManager>>,
    pub working_dir: Option<&'a Path>,
}

impl<'a> BuildToolEvalContextParams<'a> {
    pub fn new(
        config: &'a GlobalConfig,
        instance_id: &'a InstanceId,
        persistent_manager: &'a Arc<Mutex<PersistentHookManager>>,
    ) -> Self {
        Self {
            config,
            instance_id,
            agent_use_tools: None,
            current_agent_package: None,
            persistent_manager,
            working_dir: None,
        }
    }

    pub fn with_agent_use_tools(mut self, agent_use_tools: Option<&'a str>) -> Self {
        self.agent_use_tools = agent_use_tools;
        self
    }

    pub fn with_current_agent_package(mut self, package: Option<String>) -> Self {
        self.current_agent_package = package;
        self
    }
}

impl AgentLoopContext {
    pub(crate) fn tool_round_params<'a>(
        &'a self,
        config: &'a GlobalConfig,
        input: &'a Input,
        completion: CompletionText<'a>,
    ) -> ToolRoundParams<'a> {
        ToolRoundParams {
            config,
            instance_id: &self.instance_id,
            input,
            completion,
            abort_signal: &self.abort_signal,
            persistent_manager: &self.persistent_manager,
            working_dir: self.working_dir.as_deref(),
        }
    }
}

pub async fn discover_nats_tool_provider(
    config: &Config,
    instance_id: &InstanceId,
) -> Option<Arc<NatsToolProvider>> {
    NatsToolProvider::discover(
        config,
        instance_id.clone(),
        NatsInFlightCalls::for_instance(instance_id),
    )
    .await
    .ok()
    .map(Arc::new)
}
