use super::{SessionAgentSource, SessionOverrides};
use anyhow::Result;
use harnx_core::agent_config::AgentVariables;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInitializer {
    pub agent: SessionAgentSource,
    pub variables: AgentVariables,
    pub overrides: SessionOverrides,
}

impl SessionInitializer {
    pub fn named(name: impl Into<String>, variables: AgentVariables) -> Self {
        Self {
            agent: SessionAgentSource::Named { name: name.into() },
            variables,
            overrides: SessionOverrides::default(),
        }
    }

    pub fn inline(
        instructions: impl Into<String>,
        variables: AgentVariables,
        overrides: SessionOverrides,
    ) -> Self {
        Self {
            agent: SessionAgentSource::Inline {
                instructions: instructions.into(),
            },
            variables,
            overrides,
        }
    }

    pub fn agent_name(&self) -> Option<&str> {
        self.agent.name()
    }

    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        if let Some((name, _)) = &config.remote_agent {
            return Ok(Self::named(
                name,
                config.agent_variables.clone().unwrap_or_default(),
            ));
        }

        let agent = config.extract_agent();
        let variables = agent.variables().clone();
        if !agent.name().is_empty() && agent.name() != harnx_core::agent_config::TEMP_AGENT_NAME {
            return Ok(Self::named(agent.name(), variables));
        }

        let model = agent.model().id();
        anyhow::ensure!(
            !model.is_empty(),
            "inline NATS sessions require a resolved model"
        );
        Ok(Self::inline(
            agent.instructions_template(),
            variables,
            SessionOverrides {
                model: Some(model),
                temperature: agent.temperature(),
                top_p: agent.top_p(),
                use_tools: agent.use_tools(),
                model_fallbacks: agent.model_fallbacks().to_vec(),
                compress_threshold: None,
                compaction_agent: agent.compaction_agent().map(str::to_string),
                max_output_tokens: agent.model().max_output_tokens(),
            },
        ))
    }

    pub fn named_from_config(name: impl Into<String>, config: &crate::config::Config) -> Self {
        let name = name.into();
        let agent = config.extract_agent();
        if name.is_empty() || name == harnx_core::agent_config::TEMP_AGENT_NAME {
            let model = agent.model().id();
            return Self::inline(
                agent.instructions_template(),
                agent.variables().clone(),
                SessionOverrides {
                    model: (!model.is_empty()).then_some(model),
                    temperature: agent.temperature(),
                    top_p: agent.top_p(),
                    use_tools: agent.use_tools(),
                    model_fallbacks: agent.model_fallbacks().to_vec(),
                    compress_threshold: None,
                    compaction_agent: agent.compaction_agent().map(str::to_string),
                    max_output_tokens: agent.model().max_output_tokens(),
                },
            );
        }
        let variables = if config
            .remote_agent
            .as_ref()
            .is_some_and(|(remote_name, _)| remote_name == &name)
        {
            config.agent_variables.clone().unwrap_or_default()
        } else if agent.name() == name {
            agent.variables().clone()
        } else {
            AgentVariables::default()
        };
        Self::named(name, variables)
    }
}
