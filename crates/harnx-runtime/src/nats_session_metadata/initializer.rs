use super::{ParentLink, SessionAgentSource, SessionOverrides, SessionProperties, ToolContext};
use anyhow::Result;
use harnx_core::agent_config::AgentVariables;
use harnx_core::cid_url::SessionRef;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInitializer {
    pub agent: SessionAgentSource,
    pub variables: AgentVariables,
    pub overrides: SessionOverrides,
    pub tool_context: ToolContext,
    /// Properties a new session starts with: those a sub-agent session
    /// inherits from the session that started it.
    pub properties: SessionProperties,
    /// Set when this session is being created as a sub-agent child, so its
    /// metadata records the parent invocation that created it.
    pub parent: Option<ParentLink>,
}

impl SessionInitializer {
    pub fn session_key(&self, session_id: &str) -> String {
        harnx_core::session_identity::session_key(self.agent_name(), session_id)
    }

    /// Get the SessionRef for this initializer + session id.
    pub fn session_ref(&self, session_id: &str) -> SessionRef {
        SessionRef::new(
            self.agent_name().map(|s| s.to_string()),
            session_id.to_string(),
        )
        .expect("session_id is valid")
    }

    pub fn named(name: impl Into<String>, variables: AgentVariables) -> Self {
        Self {
            agent: SessionAgentSource::Named { name: name.into() },
            variables,
            overrides: SessionOverrides::default(),
            tool_context: ToolContext::default(),
            properties: SessionProperties::default(),
            parent: None,
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
            tool_context: ToolContext::default(),
            properties: SessionProperties::default(),
            parent: None,
        }
    }

    #[must_use]
    pub fn with_tool_context(mut self, tool_context: ToolContext) -> Self {
        self.tool_context = tool_context;
        self
    }

    #[must_use]
    pub fn with_properties(mut self, properties: SessionProperties) -> Self {
        self.properties = properties;
        self
    }

    /// Set an explicit identity for creation. Existing metadata is never changed by an initializer.
    #[must_use]
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        let user_id = user_id.into();
        if user_id.trim().is_empty() {
            self.properties.remove("user_id");
        } else {
            self.properties
                .put("user_id", serde_json::Value::String(user_id), None);
        }
        self
    }

    /// Fill an absent identity after parent inheritance and destination routing are resolved.
    /// Blank strings count as absent. Preserve attributes of any nonblank existing identity.
    #[must_use]
    pub fn with_default_user_id(mut self, user_id: Option<String>) -> Self {
        if self
            .properties
            .text("user_id")
            .is_some_and(|value| value.trim().is_empty())
        {
            self.properties.remove("user_id");
        }
        if self.properties.get("user_id").is_none() {
            if let Some(user_id) = user_id.filter(|value| !value.trim().is_empty()) {
                return self.with_user_id(user_id);
            }
        }
        self
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_user_id_preserves_explicit_and_inherited_properties() {
        harnx_core::require_nextest();
        let parent =
            SessionInitializer::named("parent", Default::default()).with_user_id("parent-owner");
        let inherited = parent.properties.inherited();
        let child = SessionInitializer::named("child", Default::default())
            .with_properties(inherited.clone())
            .with_default_user_id(Some("cluster-default".into()));
        assert_eq!(child.properties, inherited);
        assert_eq!(child.properties.text("user_id"), Some("parent-owner"));

        for value in [json!("explicit"), json!(null)] {
            let properties: SessionProperties = serde_json::from_value(json!({
                "user_id": {"value": value, "inherit": false, "custom": "preserved"},
                "customer": {"value": "existing", "inherit": true}
            }))
            .unwrap();
            let initializer = SessionInitializer::named("child", Default::default())
                .with_properties(properties.clone())
                .with_default_user_id(Some("cluster-default".into()));
            assert_eq!(initializer.properties, properties);
        }
    }

    #[test]
    fn blank_explicit_and_inherited_identities_allow_defaults() {
        harnx_core::require_nextest();
        for blank in ["", " \t\n"] {
            let explicit =
                SessionInitializer::named("agent", Default::default()).with_user_id(blank);
            assert!(explicit.properties.get("user_id").is_none());
            assert_eq!(
                explicit
                    .with_default_user_id(Some("default-owner".into()))
                    .properties
                    .text("user_id"),
                Some("default-owner")
            );
            let properties: SessionProperties = serde_json::from_value(json!({
                "user_id": {"value": blank, "inherit": true},
                "customer": {"value": "retained", "inherit": true}
            }))
            .unwrap();
            for inherited in [properties.clone(), properties.inherited()] {
                let child = SessionInitializer::named("child", Default::default())
                    .with_properties(inherited)
                    .with_default_user_id(Some("cluster-owner".into()));
                assert_eq!(child.properties.text("user_id"), Some("cluster-owner"));
                assert_eq!(child.properties.text("customer"), Some("retained"));
            }
            let absent = SessionInitializer::named("child", Default::default())
                .with_properties(properties)
                .with_default_user_id(None);
            assert!(absent.properties.get("user_id").is_none());
        }
    }

    #[test]
    fn default_user_id_only_adds_nonblank_identity() {
        harnx_core::require_nextest();
        for value in [None, Some("".into()), Some(" \t ".into())] {
            let initializer =
                SessionInitializer::named("agent", Default::default()).with_default_user_id(value);
            assert!(initializer.properties.get("user_id").is_none());
        }
        let initializer = SessionInitializer::named("agent", Default::default())
            .with_default_user_id(Some(" opaque owner ".into()));
        assert_eq!(
            initializer.properties.text("user_id"),
            Some(" opaque owner ")
        );
        assert!(initializer.properties.get("user_id").unwrap().inherit);
        assert!(initializer
            .properties
            .get("user_id")
            .unwrap()
            .source
            .is_none());
    }
}
