//! Reserved for config/mod.rs cluster extraction (code health). Currently unused.
use super::{update_rag, Agent, Config, GlobalConfig, Session};
use crate::client::ModelType;
use crate::nats_session_metadata::SessionOverrideUpdate;
use anyhow::{anyhow, bail, Context, Result};

impl Config {
    pub fn update(config: &GlobalConfig, data: &str) -> Result<()> {
        // `title` takes a free-form, possibly multi-word value: everything
        // after the key is the title text (not split on whitespace).
        if let Some(rest) = data.trim_start().strip_prefix("title") {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return Self::apply_update(config, "title", rest.trim());
            }
        }
        let parts: Vec<&str> = data.split_whitespace().collect();
        if parts.len() != 2 {
            bail!("Usage: .set <key> <value>. If value is null, unset key.");
        }
        Self::apply_update(config, parts[0], parts[1])
    }

    fn apply_update(config: &GlobalConfig, key: &str, value: &str) -> Result<()> {
        match key {
            "temperature" => Self::update_optional(
                config,
                value,
                SessionOverrideUpdate::Temperature,
                Config::set_temperature,
            ),
            "top_p" => Self::update_optional(
                config,
                value,
                SessionOverrideUpdate::TopP,
                Config::set_top_p,
            ),
            "use_tools" => Self::update_use_tools(config, value),
            "bash_tools" => Self::update_bash_tools(config, value),
            "model_fallbacks" => Self::update_model_fallbacks(config, value),
            "compaction_agent" => Self::update_optional(
                config,
                value,
                SessionOverrideUpdate::CompactionAgent,
                Config::set_compaction_agent,
            ),
            "max_output_tokens" => Self::update_optional(
                config,
                value,
                SessionOverrideUpdate::MaxOutputTokens,
                Config::set_max_output_tokens,
            ),
            "compress_threshold" => Self::update_optional(
                config,
                value,
                SessionOverrideUpdate::CompressThreshold,
                Config::set_compress_threshold,
            ),
            "rag_reranker_model" => {
                let value = parse_value(value)?;
                Self::set_rag_reranker_model(config, value)
            }
            "rag_top_k" => Self::update_parsed(config, value, Self::set_rag_top_k),
            "dry_run" => Self::update_bool_field(config, value, |cfg, value| cfg.dry_run = value),
            "show_sequence_numbers" => Self::update_bool_field(config, value, |cfg, value| {
                cfg.show_sequence_numbers = value
            }),
            "show_timestamps" => {
                Self::update_bool_field(config, value, |cfg, value| cfg.show_timestamps = value)
            }
            "tool_use" => Self::update_tool_use(config, value),
            "stream" => Self::update_bool_field(config, value, |cfg, value| cfg.stream = value),
            "save" => Self::update_bool_field(config, value, |cfg, value| cfg.save = value),
            "highlight" => {
                Self::update_bool_field(config, value, |cfg, value| cfg.highlight = value)
            }
            "terminal_status" => {
                Self::update_bool_field(config, value, |cfg, value| cfg.terminal_status = value)
            }
            "title" => Self::set_session_title(config, value),
            _ => bail!("Unknown key '{key}'"),
        }
    }

    /// Manually set the session title via `.set title <text>`. Appends a
    /// canonical metadata record before it updates the in-memory session, and
    /// freezes automatic regeneration by setting `title_last_updated_tokens`
    /// to `usize::MAX`. Emits `TitleUpdated`. An empty value is rejected.
    fn set_session_title(config: &GlobalConfig, value: &str) -> Result<()> {
        let title = value.trim().to_string();
        if title.is_empty() {
            bail!("Usage: .set title <text>");
        }

        let (session_id, tokens) = {
            let guard = config.read();
            let session = guard
                .session
                .as_ref()
                .context("No active session to set a title on")?;
            // Manual title: record the current token count for provenance;
            // `record_title` freezes regeneration (usize::MAX) for manual titles.
            (session.id.clone(), session.tokens)
        };
        let record = crate::config::session::TitleRecord {
            title: title.clone(),
            manual: true,
            tokens,
        };
        if !crate::config::session::record_title(config, &session_id, record)? {
            bail!("The active session changed before its title was set");
        }
        harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
            harnx_core::event::SessionEvent::TitleUpdated(title),
        ));
        Ok(())
    }

    fn update_optional<T>(
        config: &GlobalConfig,
        value: &str,
        update: impl Fn(Option<T>) -> SessionOverrideUpdate,
        setter: impl Fn(&mut Config, Option<T>),
    ) -> Result<()>
    where
        T: Clone + std::str::FromStr,
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        let value = parse_value(value)?;
        Self::persist_override(config, update(value.clone()))?;
        setter(&mut config.write(), value);
        Ok(())
    }

    fn update_parsed<T>(
        config: &GlobalConfig,
        value: &str,
        setter: impl Fn(&GlobalConfig, T) -> Result<()>,
    ) -> Result<()>
    where
        T: std::str::FromStr,
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        let value = value.parse().with_context(|| "Invalid value")?;
        setter(config, value)
    }

    /// Split a `.set` value into tool selectors, trimming blanks. Shared by
    /// `use_tools` and `bash_tools`, which differ only in how they treat
    /// `null`.
    fn split_selectors(value: &str) -> Vec<String> {
        harnx_core::agent_config::split_tool_selectors(value)
            .into_iter()
            .map(str::trim)
            .filter(|selector| !selector.is_empty())
            .map(String::from)
            .collect()
    }

    /// Shared `.set` handling for tool-selector settings: parse the value
    /// with the caller's `null` mapping, persist the session override, then
    /// apply it in memory. `use_tools` and `bash_tools` differ only in what
    /// `null` means and in which setter the value lands in.
    fn update_tool_selectors(
        config: &GlobalConfig,
        value: &str,
        parse: impl Fn(&str) -> Option<Vec<String>>,
        update: impl Fn(Option<Vec<String>>) -> SessionOverrideUpdate,
        apply: impl FnOnce(&mut Config, Option<Vec<String>>) -> Result<()>,
    ) -> Result<()> {
        let value = parse(value);
        Self::persist_override(config, update(value.clone()))?;
        apply(&mut config.write(), value)
    }

    fn update_use_tools(config: &GlobalConfig, value: &str) -> Result<()> {
        Self::update_tool_selectors(
            config,
            value,
            // `null` clears the override, so the agent's selectors apply again.
            |value| (value != "null").then(|| Self::split_selectors(value)),
            SessionOverrideUpdate::UseTools,
            |config, value| {
                config.set_use_tools(value);
                Ok(())
            },
        )
    }

    /// Unlike `use_tools`, `.set bash_tools null` is an explicit clear, not an
    /// unset override: the clear persists as an empty selector list so worker
    /// reconstruction re-applies it instead of falling back to the agent
    /// file's `bash_tools`. `SessionOverrides` uses `Option`, where `None`
    /// means "no override" (and serializes as absent).
    fn update_bash_tools(config: &GlobalConfig, value: &str) -> Result<()> {
        Self::update_tool_selectors(
            config,
            value,
            |value| {
                Some(if value == "null" {
                    Vec::new()
                } else {
                    Self::split_selectors(value)
                })
            },
            SessionOverrideUpdate::BashTools,
            Config::set_bash_tools,
        )
    }

    fn update_model_fallbacks(config: &GlobalConfig, value: &str) -> Result<()> {
        let value = if value == "null" {
            vec![]
        } else {
            value
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(String::from)
                .collect()
        };
        Self::persist_override(config, SessionOverrideUpdate::ModelFallbacks(value.clone()))?;
        config.write().set_model_fallbacks(value);
        Ok(())
    }

    fn persist_override(config: &GlobalConfig, update: SessionOverrideUpdate) -> Result<()> {
        crate::config::session::persist_active_session_override(config, &update)
    }

    fn update_bool_field(
        config: &GlobalConfig,
        value: &str,
        setter: impl Fn(&mut Config, bool),
    ) -> Result<()> {
        let value = value.parse().with_context(|| "Invalid value")?;
        setter(&mut config.write(), value);
        Ok(())
    }

    fn update_tool_use(config: &GlobalConfig, value: &str) -> Result<()> {
        let value = value.parse().with_context(|| "Invalid value")?;
        if value
            && config
                .read()
                .tool_declarations_for_use_tools(Some("*"), None)
                .0
                .is_empty()
        {
            bail!("Tool use cannot be enabled because no tools are installed.")
        }
        config.write().tool_use = value;
        Ok(())
    }

    pub fn set_temperature(&mut self, value: Option<f64>) {
        if let Some(session) = self.session.as_mut() {
            session.set_temperature(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_temperature(value);
        } else {
            self.temperature = value;
        }
    }

    pub fn set_top_p(&mut self, value: Option<f64>) {
        if let Some(session) = self.session.as_mut() {
            session.set_top_p(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_top_p(value);
        } else {
            self.top_p = value;
        }
    }

    /// Route a tool-selector setting to the active session, else the explicit
    /// agent — the precedence `.set` commands share. Returns the value
    /// untouched when neither exists, so each caller picks its own fallback:
    /// `use_tools` falls back to the global default, `bash_tools` has none
    /// and errors.
    fn set_tool_selectors(
        &mut self,
        value: Option<Vec<String>>,
        set_session: impl FnOnce(&mut Session, Option<Vec<String>>),
        set_agent: impl FnOnce(&mut Agent, Option<Vec<String>>),
    ) -> Option<Option<Vec<String>>> {
        if let Some(session) = self.session.as_mut() {
            set_session(session, value);
        } else if let Some(agent) = self.agent.as_mut() {
            set_agent(agent, value);
        } else {
            return Some(value);
        }
        None
    }

    pub fn set_use_tools(&mut self, value: Option<Vec<String>>) {
        if let Some(value) = self.set_tool_selectors(
            value,
            |session, value| session.set_use_tools(value),
            |agent, value| agent.set_use_tools(value),
        ) {
            self.use_tools = value;
        }
    }

    /// `bash_tools` has no global default (unlike `use_tools`), so without an
    /// active session or agent there is nowhere to store the value; surface
    /// that instead of silently dropping it.
    ///
    /// The value stays scoped to the active session: writing the explicit
    /// agent too would leak an override into every other session that keeps
    /// the agent (the TUI session picker switches sessions without reloading
    /// it). Tool selection still sees the override immediately through
    /// `Config::tool_selection_agent`, which overlays the session's record
    /// on the active agent.
    pub fn set_bash_tools(&mut self, value: Option<Vec<String>>) -> Result<()> {
        if self
            .set_tool_selectors(
                value,
                |session, value| session.set_bash_tools(value),
                |agent, value| agent.set_bash_tools(value),
            )
            .is_some()
        {
            bail!("no active session or agent to apply bash_tools to");
        }
        Ok(())
    }

    pub fn set_model_fallbacks(&mut self, value: Vec<String>) {
        if let Some(session) = self.session.as_mut() {
            session.set_model_fallbacks(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_model_fallbacks(value);
        }
    }

    pub fn set_compaction_agent(&mut self, value: Option<String>) {
        if let Some(session) = self.session.as_mut() {
            session.set_compaction_agent(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_compaction_agent(value);
        }
    }

    pub fn set_compress_threshold(&mut self, value: Option<usize>) {
        if let Some(session) = self.session.as_mut() {
            session.set_compress_threshold(value);
        } else {
            self.compress_threshold = value.unwrap_or_default();
        }
    }

    pub fn set_rag_reranker_model(config: &GlobalConfig, value: Option<String>) -> Result<()> {
        if let Some(id) = &value {
            crate::client::retrieve_model(&config.read().clients, id, ModelType::Reranker)?;
        }
        let has_rag = config.read().rag.is_some();
        match has_rag {
            true => update_rag(config, |rag| {
                rag.set_reranker_model(value)?;
                Ok(())
            })?,
            false => config.write().rag_reranker_model = value,
        }
        Ok(())
    }

    pub fn set_rag_top_k(config: &GlobalConfig, value: usize) -> Result<()> {
        let has_rag = config.read().rag.is_some();
        match has_rag {
            true => update_rag(config, |rag| {
                rag.set_top_k(value)?;
                Ok(())
            })?,
            false => config.write().rag_top_k = value,
        }
        Ok(())
    }

    pub fn set_wrap(&mut self, value: &str) -> Result<()> {
        if value == "no" {
            self.wrap = None;
        } else if value == "auto" {
            self.wrap = Some(value.into());
        } else {
            value
                .parse::<u16>()
                .map_err(|_| anyhow!("Invalid wrap value"))?;
            self.wrap = Some(value.into())
        }
        Ok(())
    }

    pub fn set_max_output_tokens(&mut self, value: Option<isize>) {
        if let Some(session) = self.session.as_mut() {
            let mut model = session.model().clone();
            model.set_max_tokens(value, true);
            session.set_model(model);
        } else if let Some(agent) = self.agent.as_mut() {
            let mut model = agent.model().clone();
            model.set_max_tokens(value, true);
            agent.set_model(model);
        } else {
            self.model.set_max_tokens(value, true);
        };
    }

    /// Switch the model, persisting it as the active session's override
    /// first. The override is written with no config guard held.
    pub fn switch_model(config: &GlobalConfig, model_id: &str) -> Result<()> {
        let model =
            crate::client::retrieve_model(&config.read().clients, model_id, ModelType::Chat)?;
        crate::config::session::persist_active_session_override(
            config,
            &SessionOverrideUpdate::Model(Some(model.id())),
        )?;
        config.write().apply_model(model);
        Ok(())
    }

    /// Set the model in memory only. Callers that change a session's model
    /// on the user's behalf use [`Config::switch_model`], which also persists
    /// the override.
    pub fn set_model(&mut self, model_id: &str) -> Result<()> {
        let model = crate::client::retrieve_model(&self.clients, model_id, ModelType::Chat)?;
        self.apply_model(model);
        Ok(())
    }

    fn apply_model(&mut self, model: crate::client::Model) {
        if let Some(session) = self.session.as_mut() {
            session.set_model(model);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_model(model);
        } else {
            self.model = model;
        }
    }
}

fn parse_value<T: std::str::FromStr>(value: &str) -> Result<Option<T>>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    if value == "null" {
        Ok(None)
    } else {
        Ok(Some(value.parse().with_context(|| "Invalid value")?))
    }
}

#[cfg(test)]
mod title_command_tests {
    use super::super::*;
    use crate::config::ConfigLock;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn config_with_session(_dir: &std::path::Path) -> GlobalConfig {
        let mut config = Config::default();
        let mut session = crate::config::session::new(&config, "title-test", None).unwrap();
        crate::config::session::attach_memory_log(&mut session);
        config.session = Some(session);
        Arc::new(ConfigLock::new(config))
    }

    #[test]
    fn set_title_multiword_updates_session_and_freezes_regeneration() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_session(tmp.path());

        Config::update(&config, "title My Custom Session Title").unwrap();

        let guard = config.read();
        let session = guard.session.as_ref().unwrap();
        assert_eq!(session.title(), Some("My Custom Session Title"));
        // Frozen: auto-regeneration is disabled after a manual title.
        assert_eq!(session.title_last_updated_tokens(), usize::MAX);
        assert!(!session.need_generate_title(50_000));
    }

    #[test]
    fn set_title_empty_value_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_session(tmp.path());
        assert!(Config::update(&config, "title   ").is_err());
    }
}

#[cfg(test)]
mod bash_tools_command_tests {
    use super::super::*;
    use crate::config::ConfigLock;
    use std::sync::Arc;

    fn config_with_session() -> GlobalConfig {
        let mut config = Config::default();
        let mut session = crate::config::session::new(&config, "bash-tools-test", None).unwrap();
        crate::config::session::attach_memory_log(&mut session);
        config.session = Some(session);
        Arc::new(ConfigLock::new(config))
    }

    #[test]
    fn set_bash_tools_updates_session_and_clears_with_null() {
        let config = config_with_session();

        Config::update(&config, "bash_tools fs_*,grep_search").unwrap();

        let guard = config.read();
        let session = guard.session.as_ref().unwrap();
        assert_eq!(
            session.bash_tools(),
            Some(vec!["fs_*".to_string(), "grep_search".to_string()])
        );
        drop(guard);

        Config::update(&config, "bash_tools null").unwrap();

        let guard = config.read();
        let session = guard.session.as_ref().unwrap();
        // `null` clears script mode and must be distinguishable from an unset
        // override: the persisted record is an empty list, not absent, so a
        // reconstructed worker re-applies the clear.
        assert_eq!(session.bash_tools(), Some(Vec::new()));
    }

    /// A config with an explicit agent (its own `bash_tools`), an active
    /// session, and declarations the selectors can resolve against.
    fn config_with_agent_session_and_tools() -> Config {
        let declaration = |name: &str, description: &str| ToolDeclaration {
            name: name.to_string(),
            description: description.to_string(),
            parameters: Default::default(),
            mcp_tool_name: None,
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
            kind: None,
        };
        let mut config = Config {
            tools: Tools::init_from_mcp(Some(vec![
                declaration("fs_read", "read"),
                declaration("grep_search", "grep"),
            ])),
            ..Config::default()
        };
        let mut agent = crate::config::Agent::default();
        agent.set_bash_tools(Some(vec!["grep_search".to_string()]));
        config.use_agent_obj(agent).unwrap();
        let mut session = crate::config::session::new(&config, "bash-tools-a", None).unwrap();
        crate::config::session::attach_memory_log(&mut session);
        config.session = Some(session);
        config
    }

    #[test]
    fn set_bash_tools_stays_scoped_to_the_active_session() {
        let config = Arc::new(ConfigLock::new(config_with_agent_session_and_tools()));

        // While session A is active, the override steers tool selection even
        // though `extract_agent` prefers the explicit agent: tool selection
        // resolves through `tool_selection_agent`, which overlays the
        // session's record on the active agent.
        Config::update(&config, "bash_tools fs_*").unwrap();
        {
            let guard = config.read();
            assert_eq!(
                guard.session.as_ref().unwrap().bash_tools(),
                Some(vec!["fs_*".to_string()])
            );
            let agent = guard.tool_selection_agent();
            assert_eq!(guard.bash_tool_names(&agent), vec!["fs_read"]);
        }

        // The explicit agent keeps its own selectors, so switching sessions
        // (the TUI picker path: `use_session` keeps the agent) does not carry
        // session A's override into session B — B snapshots the agent.
        config.write().use_session(Some("bash-tools-b")).unwrap();
        let guard = config.read();
        let agent = guard.tool_selection_agent();
        assert_eq!(
            guard.extract_agent().bash_tools(),
            Some(vec!["grep_search".to_string()]),
            "the override must not overwrite the explicit agent"
        );
        assert_eq!(
            guard.session.as_ref().unwrap().bash_tools(),
            Some(vec!["grep_search".to_string()])
        );
        assert_eq!(guard.bash_tool_names(&agent), vec!["grep_search"]);
    }

    #[test]
    fn set_bash_tools_errors_without_session_or_agent() {
        // No session, no agent: there is nowhere to store the value, so the
        // command must fail instead of silently doing nothing.
        let config = Arc::new(ConfigLock::new(Config::default()));

        let error = Config::update(&config, "bash_tools fs_*").unwrap_err();

        assert!(
            error.to_string().contains("no active session or agent"),
            "unexpected error: {error}"
        );
    }
}
