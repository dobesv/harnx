//! Agent lifecycle methods extracted from config/mod.rs for code health.
use super::*;

struct UseRemoteAgentParams<'a> {
    config: &'a GlobalConfig,
    agent: &'a str,
    cluster: &'a str,
    session_name: Option<&'a str>,
    _abort_signal: AbortSignal,
}

use harnx_core::agent_ref::AgentRef;

fn local_agent_configs() -> HashMap<String, AgentConfig> {
    self::agent::list_local_agent_names()
        .into_iter()
        .chain(self::agent::list_package_agent_names())
        .filter_map(|name| {
            let path = Config::agent_file(&name);
            self::agent::load_with_qualified_name(&path, &name)
                .ok()
                .map(|agent| (name, agent.into_config()))
        })
        .collect()
}

fn remote_agent_configs(servers: &[NatsServerConfig]) -> HashMap<String, AgentConfig> {
    servers
        .iter()
        .flat_map(|server| {
            server.agents.iter().map(move |remote| {
                let name = format!("{}@{}", remote.name, server.name);
                let agent = AgentConfig::from_remote_catalog(
                    &name,
                    remote.description.clone(),
                    remote.role,
                );
                (name, agent)
            })
        })
        .collect()
}

/// Keep canonical top-level references explicitly absolute while package
/// patches run. Otherwise an unchanged `local:small` is indistinguishable
/// from a newly assigned package-relative target with the same spelling.
fn prepare_patch_model_ids(agent: &mut Agent) {
    if harnx_core::package_namespace::pkg_from_qualified(agent.name()).is_none() {
        return;
    }
    let file_form = |id: &str| {
        let client = id.split_once(':').map_or(id, |(client, _)| client);
        if client.contains('/') {
            id.to_string()
        } else {
            format!("/{id}")
        }
    };
    let model_id = agent.model_id().map(file_form);
    let fallbacks = agent
        .model_fallbacks()
        .iter()
        .map(|id| file_form(id))
        .collect();
    agent.set_model_id(model_id);
    agent.set_model_fallbacks(fallbacks);
}

fn resolve_patched_agent_variables(
    agent: &mut Agent,
    baseline: &Agent,
    variables: &AgentVariables,
) -> Result<()> {
    // Compare the declared shape, excluding serde-skipped scratch values.
    if serde_json::to_value(agent.defined_variables())?
        != serde_json::to_value(baseline.defined_variables())?
    {
        self::agent::resolve_session_variables(agent, variables)?;
    }
    // Validate against session inputs, not cached defaults, even for an
    // unchanged agent. Required values may only be supplied by the final shape.
    self::agent::require_agent_variables(agent.defined_variables(), variables)?;
    Ok(())
}

impl Config {
    /// Set the agent and cluster for the active session.
    pub fn set_remote_agent(&mut self, agent: String, cluster: String) {
        // Agent switches cannot carry another namespace's session or local
        // agent configuration into the next prompt/cancellation target.
        self.session = None;
        self.agent = None;
        self.rag = None;
        self.discontinuous_last_message();
        self.remote_agent = Some((agent, cluster));
    }

    /// Selected agent reference for frontend navigation, including its cluster.
    pub fn active_agent_ref(&self) -> Option<String> {
        self.remote_agent
            .as_ref()
            .map(|(agent, cluster)| format!("{agent}@{cluster}"))
            .or_else(|| self.agent.as_ref().map(|agent| agent.name().to_string()))
    }

    /// Commit a prepared frontend selection without dropping non-cloneable
    /// terminal lifecycle hooks from the live config.
    pub fn apply_prepared_agent_selection(&mut self, mut prepared: Config) {
        prepared.tui_before_editor = self.tui_before_editor.take();
        prepared.tui_after_editor = self.tui_after_editor.take();
        *self = prepared;
    }

    /// Check whether the active agent ref names a cluster.
    pub fn is_remote_agent(&self) -> bool {
        self.remote_agent.is_some()
    }

    pub fn use_prompt(&mut self, prompt: &str) -> Result<()> {
        let mut agent = Agent::new(AgentConfig::from_prompt(prompt));
        agent.set_model(self.current_model().clone());
        self.inherit_agent_defaults(&mut agent);
        self.use_agent_obj(agent)
    }

    fn inherit_agent_defaults(&self, agent: &mut Agent) {
        Self::apply_agent_default_overrides(
            agent,
            self.temperature,
            self.top_p,
            self.use_tools.clone(),
        );
    }

    /// The `inherit_agent_defaults` logic without a config borrow, so callers
    /// holding a mutable agent borrow can snapshot the fallbacks first.
    fn apply_agent_default_overrides(
        agent: &mut Agent,
        temperature: Option<f64>,
        top_p: Option<f64>,
        use_tools: Option<Vec<String>>,
    ) {
        let temperature = agent.temperature().or(temperature);
        let top_p = agent.top_p().or(top_p);
        let use_tools = agent.use_tools().or(use_tools);
        agent.set_temperature(temperature);
        agent.set_top_p(top_p);
        agent.set_use_tools(use_tools);
    }

    /// Turn the active agent into the session's effective agent: stamp the
    /// caller-principals snapshot onto it and run the agent's `patches` with
    /// `$user` bound to that snapshot.
    ///
    /// The caller runs this after session overrides (so an override cannot
    /// re-add what a patch removed) and before anything reads the agent —
    /// server selection, hooks, middleware, run limits, prompt rendering. All
    /// of those read `Config.agent`, so the patched agent is what they see.
    /// `variables` must be the original session inputs, not the resolved map,
    /// so changed declarations can supply new defaults without losing explicit values.
    ///
    /// Fail-closed (G6): the error already names the agent and the failing
    /// filter, and the caller must fail the activation; running the unpatched
    /// config is never allowed.
    // qual:allow(iosp) — atomic activation pipeline: patch, resolve, validate, then publish; keeping the steps together makes failure leave the active config untouched
    pub fn apply_session_agent_patches(
        &mut self,
        user: &harnx_core::user_principals::UserPrincipals,
        variables: &AgentVariables,
    ) -> Result<()> {
        let Some(baseline) = self.agent.as_ref() else {
            bail!("cannot apply session agent `patches`: no active agent");
        };
        // Publish only after resolution and validation succeed, including
        // variable defaults introduced by a patch.
        let mut agent = baseline.clone();
        agent.set_user_principals(Some(user.clone()));
        if !agent.patches().is_empty() {
            prepare_patch_model_ids(&mut agent);
            agent.apply_user_patches(user)?;
            self.resolve_patched_agent_model(&mut agent, baseline)
                .with_context(|| {
                    format!("agent '{}': resolve model after `patches`", agent.name())
                })?;
        }
        resolve_patched_agent_variables(&mut agent, baseline, variables).with_context(|| {
            format!(
                "agent '{}': resolve variables after `patches`",
                agent.name()
            )
        })?;
        // Re-validate middleware against the patched agent, mirroring
        // `use_agent_obj`: a patch that adds an invalid middleware entry must
        // fail the activation, not the first tool call.
        self.resolved_middleware_for_agent(Some(&agent))
            .with_context(|| {
                format!(
                    "agent '{}': validate middleware after `patches`",
                    agent.name()
                )
            })?;
        self.agent = Some(agent);
        Ok(())
    }

    /// Re-resolve the model after `patches` changed the agent's `model` id,
    /// mirroring [`Config::retrieve_agent`]. An unchanged id keeps the runtime
    /// model the patch already restored; a cleared id falls back to the
    /// global model and inherited defaults.
    fn resolve_patched_agent_model(&self, agent: &mut Agent, baseline: &Agent) -> Result<()> {
        let package = harnx_core::package_namespace::pkg_from_qualified(agent.name());
        let model_id = agent
            .model_id()
            .map(|id| self::agent::resolve_package_model_id(id, package));
        let fallbacks = agent
            .model_fallbacks()
            .iter()
            .map(|id| self::agent::resolve_package_model_id(id, package))
            .collect();
        agent.set_model_id(model_id);
        agent.set_model_fallbacks(fallbacks);
        if agent.model_id() == baseline.model_id() {
            // The patch's file-form id may differ from the canonical id even
            // when both name the same model. Keep its resolved runtime data.
            agent.set_resolved_model(baseline.model().clone());
            return Ok(());
        }
        match agent.model_id() {
            Some(model_id) => {
                let model =
                    crate::client::retrieve_model(&self.clients, model_id, ModelType::Chat)?;
                agent.set_model(model);
            }
            None => {
                // current_model() would read the active agent, not the global fallback.
                agent.set_model(self.model.clone());
                self.inherit_agent_defaults(agent);
            }
        }
        Ok(())
    }

    pub fn retrieve_agent(&self, name: &str) -> Result<Agent> {
        let path = Self::agent_file(name);
        let mut agent = if path.exists() {
            self::agent::load_with_qualified_name(&path, name)?
        } else {
            self::agent::builtin(name)?
        };
        self.resolved_middleware_for_agent(Some(&agent))?;
        self.resolved_event_hooks_for_agent(Some(&agent))?;
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
                self.inherit_agent_defaults(&mut agent);
            }
        }
        Ok(agent)
    }

    fn remote_agent_target(&self, name: &str) -> Option<(String, String)> {
        match AgentRef::parse(name) {
            AgentRef::Remote { agent, cluster } => Some((agent.into_owned(), cluster.into_owned())),
            AgentRef::Local(agent) => self
                .default_cluster_for_display()
                .map(|cluster| (agent.into_owned(), cluster.to_string())),
        }
    }

    pub fn use_agent_by_name(&mut self, name: &str) -> Result<()> {
        if let Some((agent, cluster)) = self.remote_agent_target(name) {
            self.nats_server(&cluster)
                .map_err(|e| anyhow::anyhow!("remote agent validation failed: {e}"))?;
            self.set_remote_agent(agent, cluster);
            return Ok(());
        }
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

    /// Install an already-built agent as the active agent.
    pub(super) fn set_active_agent(&mut self, agent: Agent) {
        self.remote_agent = None;
        self.session = None;
        self.rag = agent.rag();
        self.discontinuous_last_message();
        self.agent = Some(agent);
    }

    pub fn use_agent_obj(&mut self, agent: Agent) -> Result<()> {
        self.resolved_middleware_for_agent(Some(&agent))?;
        self.resolved_event_hooks_for_agent(Some(&agent))?;
        self.exit_active_agent()?;
        self.set_active_agent(agent);
        Ok(())
    }

    fn exit_active_agent(&mut self) -> Result<()> {
        if self.agent.is_some() {
            self.exit_agent()?;
        }
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

    pub fn all_agents(&self) -> Vec<AgentConfig> {
        let mut agents = local_agent_configs();
        agents.extend(remote_agent_configs(&self.nats_servers));
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
        let remote_target = config.read().remote_agent_target(agent_name);
        if let Some((agent, cluster)) = remote_target {
            Self::use_remote_agent(UseRemoteAgentParams {
                config,
                agent: &agent,
                cluster: &cluster,
                session_name,
                _abort_signal: abort_signal,
            })
            .await
        } else {
            Self::use_local_agent(config, agent_name, session_name, abort_signal).await
        }
    }

    /// Activate an agent for the frontend's next turn.
    async fn use_remote_agent(params: UseRemoteAgentParams<'_>) -> Result<()> {
        let UseRemoteAgentParams {
            config,
            agent,
            cluster,
            session_name,
            _abort_signal,
        } = params;
        // For now, remote agents are activated lazily when the first prompt arrives.
        // We just validate the cluster exists and prepare metadata.
        {
            let cfg = config.read();
            // Validate cluster exists
            cfg.nats_server(cluster)
                .map_err(|e| anyhow::anyhow!("remote agent validation failed: {e}"))?;
        }

        // Store remote agent metadata in config for use during prompt processing
        // The actual NatsSession is created when the user sends a prompt
        let mut cfg = config.write();
        cfg.set_remote_agent(agent.to_string(), cluster.to_string());
        if let Some(session) = session_name {
            cfg.use_session(Some(session))?;
        }

        Ok(())
    }

    async fn use_local_agent(
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
        // Tools are now loaded via NATS tool_servers, not direct MCP
        let agent = self::agent::init(config, agent_name, abort_signal).await?;
        let session = session_name.map(|v| v.to_string());
        config.write().set_active_agent(agent);
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
        }
        // Clear remote agent metadata too
        self.remote_agent.take();
        Ok(())
    }
}
