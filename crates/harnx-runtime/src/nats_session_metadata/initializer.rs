use super::{
    ParentLink, SessionAgentSource, SessionMetadataPatch, SessionOverrides, SessionProperties,
    ToolContext,
};
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
    /// Metadata a client supplied with the session-creation request, applied
    /// to the initial canonical record before it is first persisted. The
    /// initializer is not serialized, so this never reaches the wire schema.
    pub metadata_patch: Option<SessionMetadataPatch>,
}

// qual:allow(dry, boilerplate) reason: "Set-and-return builders are the established pattern for SessionInitializer; adding a builder-derive dependency for three trivial setters costs more than the pattern."
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
            metadata_patch: None,
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
            metadata_patch: None,
        }
    }

    /// Supply the metadata this session is created with, applied to the initial
    /// canonical record before it is first persisted. Field semantics match a
    /// metadata PATCH: supplied variables and overrides replace the
    /// initializer-derived values, omitted fields keep them, and a title is
    /// stored trimmed with `manual` honored only when it is nonempty.
    #[must_use]
    pub fn with_metadata_patch(mut self, patch: SessionMetadataPatch) -> Self {
        self.metadata_patch = Some(patch);
        self
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

    /// Record the caller principals this session is created for. Only
    /// trusted server-side creation code calls this; the property is
    /// read-only for agents and clients, and an inherited snapshot is
    /// never recomputed.
    #[must_use]
    pub fn with_user_principals(
        mut self,
        principals: &harnx_core::user_principals::UserPrincipals,
    ) -> Self {
        self.properties.put(
            super::session_properties::USER_PRINCIPALS_PROPERTY,
            principals.to_property_value(),
            None,
        );
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
                self = self.with_user_id(user_id);
            }
        }
        // Deliberately no principals fallback here: a session without an
        // explicit snapshot reads through `SessionProperties::user_principals`,
        // which derives the same legacy shape from the user_id property.
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
                bash_tools: agent.bash_tools(),
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
                    bash_tools: agent.bash_tools(),
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
    use harnx_core::user_principals::{from_resolved, legacy_fallback};
    use serde_json::json;

    #[test]
    fn default_user_id_leaves_principals_absent_and_reads_the_legacy_fallback() {
        harnx_core::require_nextest();
        // CLI/TUI/ACP sessions stamp no snapshot; the accessor derives the
        // legacy shape from the user_id property instead.
        let initializer = SessionInitializer::named("agent", Default::default())
            .with_default_user_id(Some("local-owner".into()));
        assert!(initializer.properties.get("user_principals").is_none());
        assert_eq!(
            initializer.properties.user_principals(),
            legacy_fallback(Some("local-owner"))
        );

        // An inherited snapshot is never recomputed from a cluster default.
        let parent = SessionInitializer::named("parent", Default::default())
            .with_user_id("parent-owner")
            .with_user_principals(&from_resolved(
                Some("parent-owner"),
                Some(&["parent-owner".into(), "alias".into()]),
                &["developers".into()],
                &["reviewer".into()],
            ));
        let child = SessionInitializer::named("child", Default::default())
            .with_properties(parent.properties.inherited())
            .with_default_user_id(Some("cluster-default".into()));
        assert_eq!(
            child.properties.user_principals(),
            parent.properties.user_principals()
        );

        // Anonymous sessions keep no snapshot and read anonymous.
        let anonymous = SessionInitializer::named("agent", Default::default())
            .with_default_user_id(Some("  ".into()));
        assert!(anonymous.properties.get("user_principals").is_none());
        assert_eq!(
            anonymous.properties.user_principals(),
            harnx_core::user_principals::anonymous()
        );
    }

    #[test]
    fn with_user_principals_stamps_the_snapshot() {
        harnx_core::require_nextest();
        let principals = from_resolved(
            Some("alice"),
            Some(&["alice".into(), "bob".into()]),
            &["engineering".into()],
            &["reviewer".into()],
        );
        let initializer = SessionInitializer::named("agent", Default::default())
            .with_user_principals(&principals);
        assert_eq!(
            initializer.properties.get("user_principals").unwrap().value,
            principals.to_property_value()
        );
        assert!(
            initializer
                .properties
                .get("user_principals")
                .unwrap()
                .inherit
        );
        // Metadata built from the initializer round-trips the snapshot.
        let metadata = crate::nats_session_metadata::SessionMetadata::new("session-1", initializer);
        assert_eq!(
            crate::nats_session_metadata::user_principals(&metadata),
            principals
        );
    }

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
