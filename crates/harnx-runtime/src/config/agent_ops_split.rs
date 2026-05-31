//! Agent lifecycle methods extracted from config/mod.rs for code health.
use super::*;

impl Config {
    pub fn use_prompt(&mut self, prompt: &str) -> Result<()> {
        let mut agent = Agent::new(AgentConfig::from_prompt(prompt));
        agent.set_model(self.current_model().clone());
        if agent.temperature().is_none() {
            agent.set_temperature(self.temperature);
        }
        if agent.top_p().is_none() {
            agent.set_top_p(self.top_p);
        }
        if agent.use_tools().is_none() {
            agent.set_use_tools(self.use_tools.clone());
        }
        self.use_agent_obj(agent)
    }

    pub fn retrieve_agent(&self, name: &str) -> Result<Agent> {
        let path = Self::agent_file(name);
        let mut agent = if path.exists() {
            self::agent::load_with_qualified_name(&path, name)?
        } else {
            self::agent::builtin(name)?
        };
        let current_model = self.current_model().clone();
        match agent.model_id() {
            Some(model_id) => {
                if current_model.id() != model_id {
                    let model =
                        crate::client::retrieve_model(&self.clients, model_id, ModelType::Chat)?;
                    agent.set_model(model);
                } else {
                    agent.set_model(current_model);
                }
            }
            None => {
                agent.set_model(current_model);
                if agent.temperature().is_none() {
                    agent.set_temperature(self.temperature);
                }
                if agent.top_p().is_none() {
                    agent.set_top_p(self.top_p);
                }
                if agent.use_tools().is_none() {
                    agent.set_use_tools(self.use_tools.clone());
                }
            }
        }
        Ok(agent)
    }

    pub fn use_agent_by_name(&mut self, name: &str) -> Result<()> {
        let mut agent = self.retrieve_agent(name)?;
        // Mirror the async `use_agent` flow: `init()` resolves file-backed
        // variable defaults (the `path:` field) before the agent becomes
        // active.  Without this, a follow-up `use_session` would call
        // `init_agent_session_variables`, find unresolved required variables,
        // and bail with "agent variables are required".
        self::agent::resolve_file_defaults(&mut agent)?;
        // Populate shared_variables from the resolved defaults so that
        // `session::new()` -> `set_agent()` -> `render_template()` can access
        // user-defined variables immediately (before `init_agent_session_variables`
        // runs). This mirrors the variable-initialization step in
        // `init_agent_session_variables` for the no-session-yet case.
        if !agent.defined_variables().is_empty() && agent.shared_variables().is_empty() {
            let mut config_variables = AgentVariables::default();
            if let Some(v) = &self.agent_variables {
                config_variables.extend(v.clone());
            }
            let shared = self::agent::init_agent_variables(
                agent.defined_variables(),
                &config_variables,
                self.info_flag,
            )?;
            agent.set_shared_variables(shared);
        }
        self.use_agent_obj(agent)
    }

    pub fn use_agent_obj(&mut self, agent: Agent) -> Result<()> {
        if self.agent.is_some() {
            self.exit_agent()?;
        }

        // Reinitialize MCP/ACP managers scoped to the incoming agent's package
        // (or None for top-level agents). This gives the agent a clean view:
        // same-package MCP servers appear under their bare names, others prefixed.
        let agent_package =
            harnx_core::package_namespace::pkg_from_qualified(agent.name()).map(str::to_string);
        self.reinit_managers_for_agent(agent_package.as_deref());

        self.agent = Some(agent);
        Ok(())
    }

    pub fn edit_agent_prompt(&mut self) -> Result<()> {
        let agent_name;
        if let Some(session) = self.session.as_ref() {
            if let Some(name) = session.agent_name().map(|v| v.to_string()) {
                if session.is_empty() {
                    agent_name = Some(name);
                } else {
                    bail!("Cannot perform this operation because you are in a non-empty session")
                }
            } else {
                bail!("No agent")
            }
        } else {
            agent_name = self.agent.as_ref().map(|v| v.name().to_string());
        }
        let name = agent_name.ok_or_else(|| anyhow!("No agent"))?;
        self.upsert_agent(&name)?;
        self.use_agent_by_name(&name)
    }

    pub fn upsert_agent(&mut self, name: &str) -> Result<()> {
        let agent_path = Self::agent_file(name);
        ensure_parent_exists(&agent_path)?;
        self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &agent_path)
        })?;
        if self.working_mode.is_tui() {
            crate::utils::emit_info(format!("✓ Saved the agent to '{}'.", agent_path.display()));
        }
        Ok(())
    }

    pub fn save_agent(&mut self, name: Option<&str>) -> Result<()> {
        let mut agent_name = match &self.agent {
            Some(agent) => {
                if agent.has_args() {
                    bail!("Unable to save the agent with arguments (whose name contains '#')")
                }
                match name {
                    Some(v) => v.to_string(),
                    None => agent.name().to_string(),
                }
            }
            None => bail!("No agent"),
        };
        if agent_name == TEMP_AGENT_NAME {
            agent_name = Text::new("Agent name:")
                .with_validator(|input: &str| {
                    let input = input.trim();
                    if input.is_empty() {
                        Ok(Validation::Invalid("This name is required".into()))
                    } else if input == TEMP_AGENT_NAME {
                        Ok(Validation::Invalid("This name is reserved".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt()?;
        }
        let agent_path = Self::agent_file(&agent_name);
        if let Some(agent) = self.agent.as_mut() {
            let content = agent.export()?;
            ensure_parent_exists(&agent_path)?;
            std::fs::write(&agent_path, content).with_context(|| {
                format!(
                    "Failed to write agent '{}' to '{}'",
                    agent.name(),
                    agent_path.display()
                )
            })?;
            agent.set_name(&agent_name);
            if self.working_mode.is_tui() {
                crate::utils::emit_info(format!(
                    "✓ Saved the agent to '{}'.",
                    agent_path.display()
                ));
            }
        }

        Ok(())
    }

    pub fn all_agents() -> Vec<AgentConfig> {
        let mut agents: HashMap<String, AgentConfig> = HashMap::new();
        for name in list_agents() {
            let path = Self::agent_file(&name);
            if let Ok(agent) = self::agent::load_with_qualified_name(&path, &name) {
                agents.insert(name, agent.into_config());
            }
        }
        let mut agents: Vec<_> = agents.into_values().collect();
        agents.sort_unstable_by(|a, b| a.name().cmp(b.name()));
        agents
    }

    pub async fn use_agent(
        config: &GlobalConfig,
        agent_name: &str,
        session_name: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if !config.read().tool_use {
            bail!("Please enable tool use before using the agent.");
        }
        if config.read().agent.is_some() {
            config.write().exit_agent()?;
        }
        let agent = self::agent::init(config, agent_name, abort_signal).await?;
        let session = session_name.map(|v| v.to_string());
        config.write().rag = agent.rag();
        config.write().agent = Some(agent);
        // Populate shared_variables from resolved file-backed defaults and
        // any --agent-variable overrides before any code path that renders
        // the template. session::new() -> set_agent() runs the template
        // immediately, so this must happen before use_session().
        config.write().init_agent_shared_variables()?;
        if let Some(session) = session {
            // Exit any existing session before
            // switching to the agent's session.
            config.write().exit_session()?;
            config.write().use_session(Some(&session))?;
        }
        Ok(())
    }

    pub fn agent_info(&self) -> Result<String> {
        if let Some(agent) = &self.agent {
            agent.export()
        } else {
            bail!("No agent")
        }
    }

    pub fn agent_banner(&self) -> Result<String> {
        if let Some(agent) = &self.agent {
            Ok(agent.banner())
        } else {
            bail!("No agent")
        }
    }

    pub fn exit_agent(&mut self) -> Result<()> {
        self.exit_session()?;
        if self.agent.take().is_some() {
            self.rag.take();
            self.discontinuous_last_message();
            // Restore global (no-agent) manager view: all package servers prefixed.
            self.reinit_managers_for_agent(None);
        }
        Ok(())
    }
}
