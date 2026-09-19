use crate::{ag_ui::AgUiError, session_actor::ResolvedAgentTarget};
use harnx_core::{abort::create_abort_signal, agent_ref::AgentRef};
use harnx_runtime::config::{AgentConfig, Config, GlobalConfig};

pub(crate) fn is_safe_path_segment(value: &str) -> bool {
    !value.is_empty()
        && std::path::Path::new(value)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && !value.contains(['/', '\\'])
}

pub(crate) fn is_safe_agent_path(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('/') && value.split('/').all(is_safe_path_segment)
}

pub(crate) async fn resolve_agent_target(
    config: &Config,
    agent_ref: &str,
) -> Result<(ResolvedAgentTarget, GlobalConfig), AgUiError> {
    validate_agent_reference(config, agent_ref)?;
    let scoped = harnx_session::fork_prompt_config(config);
    Config::use_agent(&scoped, agent_ref, None, create_abort_signal())
        .await
        .map_err(|err| {
            AgUiError::Internal(format!("failed to resolve agent '{agent_ref}': {err:#}"))
        })?;
    let target = resolved_target(&scoped, agent_ref)?;
    Ok((target, scoped))
}

fn resolved_target(
    scoped: &GlobalConfig,
    agent_ref: &str,
) -> Result<ResolvedAgentTarget, AgUiError> {
    let config = scoped.read();
    if let Some((agent, cluster)) = config.remote_agent.clone() {
        return Ok(ResolvedAgentTarget::new(agent, cluster));
    }
    config
        .agent
        .as_ref()
        .map(|agent| ResolvedAgentTarget::local(agent.name().to_string()))
        .ok_or_else(|| {
            AgUiError::Internal(format!("agent resolver did not activate '{agent_ref}'"))
        })
}

/// Validates path safety after parsing so encoded remote refs cannot hide an unsafe bare agent.
fn validate_agent_reference(config: &Config, agent_ref: &str) -> Result<(), AgUiError> {
    let not_found = || AgUiError::NotFound(format!("agent '{agent_ref}' not found"));
    match AgentRef::parse(agent_ref) {
        AgentRef::Local(agent) => {
            if !is_safe_agent_path(agent.as_ref()) {
                return Err(not_found());
            }
            let exists = Config::agent_file(agent.as_ref()).exists()
                || AgentConfig::builtin_markdown(agent.as_ref()).is_some();
            exists.then_some(()).ok_or_else(not_found)
        }
        AgentRef::Remote { agent, cluster } => {
            if !is_safe_agent_path(agent.as_ref()) {
                return Err(not_found());
            }
            config
                .nats_server(cluster.as_ref())
                .map(|_| ())
                .map_err(|err| AgUiError::BadRequest(err.to_string()))
        }
    }
}
