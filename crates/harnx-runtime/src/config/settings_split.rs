//! Reserved for config/mod.rs cluster extraction (code health). Currently unused.
use super::{update_rag, Config, GlobalConfig};
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

        {
            let mut guard = config.write();
            let session = guard
                .session
                .as_mut()
                .context("No active session to set a title on")?;
            // Manual title: record the current token count for provenance;
            // `record_title` freezes regeneration (usize::MAX) for manual titles.
            let tokens = session.tokens;
            crate::config::session::record_title(session, title.clone(), true, tokens)?;
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

    fn update_use_tools(config: &GlobalConfig, value: &str) -> Result<()> {
        let value = if value == "null" {
            None
        } else {
            Some(
                harnx_core::agent_config::split_tool_selectors(value)
                    .into_iter()
                    .map(str::trim)
                    .filter(|selector| !selector.is_empty())
                    .map(String::from)
                    .collect(),
            )
        };
        Self::persist_override(config, SessionOverrideUpdate::UseTools(value.clone()))?;
        config.write().set_use_tools(value);
        Ok(())
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
        let guard = config.read();
        let Some(session) = guard.session.as_ref() else {
            return Ok(());
        };
        crate::config::session::persist_session_override(session, &update)
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

    pub fn set_use_tools(&mut self, value: Option<Vec<String>>) {
        if let Some(session) = self.session.as_mut() {
            session.set_use_tools(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_use_tools(value);
        } else {
            self.use_tools = value;
        }
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

    pub fn set_model(&mut self, model_id: &str) -> Result<()> {
        let model = crate::client::retrieve_model(&self.clients, model_id, ModelType::Chat)?;
        if let Some(session) = self.session.as_mut() {
            crate::config::session::persist_session_override(
                session,
                &SessionOverrideUpdate::Model(Some(model.id())),
            )?;
            session.set_model(model);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_model(model);
        } else {
            self.model = model;
        }
        Ok(())
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
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn config_with_session(_dir: &std::path::Path) -> GlobalConfig {
        let mut config = Config::default();
        let mut session = crate::config::session::new(&config, "title-test", None).unwrap();
        crate::config::session::attach_memory_log(&mut session);
        config.session = Some(session);
        Arc::new(RwLock::new(config))
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
