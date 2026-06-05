//! Reserved for config/mod.rs cluster extraction (code health). Currently unused.
use super::{update_rag, Config, GlobalConfig};
use crate::client::ModelType;
use anyhow::{anyhow, bail, Context, Result};

impl Config {
    pub fn update(config: &GlobalConfig, data: &str) -> Result<()> {
        let parts: Vec<&str> = data.split_whitespace().collect();
        if parts.len() != 2 {
            bail!("Usage: .set <key> <value>. If value is null, unset key.");
        }
        Self::apply_update(config, parts[0], parts[1])
    }

    fn apply_update(config: &GlobalConfig, key: &str, value: &str) -> Result<()> {
        match key {
            "temperature" => Self::update_optional_f64(config, value, Config::set_temperature),
            "top_p" => Self::update_optional_f64(config, value, Config::set_top_p),
            "use_tools" => Self::update_use_tools(config, value),
            "model_fallbacks" => Self::update_model_fallbacks(config, value),
            "compaction_agent" => {
                Self::update_optional_string(config, value, Config::set_compaction_agent)
            }
            "max_output_tokens" => {
                Self::update_optional_isize(config, value, Config::set_max_output_tokens)
            }
            "save_session" => Self::update_optional_bool(config, value, Config::set_save_session),
            "compress_threshold" => {
                Self::update_optional_usize(config, value, Config::set_compress_threshold)
            }
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
            _ => bail!("Unknown key '{key}'"),
        }
    }

    fn update_optional_f64(
        config: &GlobalConfig,
        value: &str,
        setter: impl Fn(&mut Config, Option<f64>),
    ) -> Result<()> {
        let value = parse_value(value)?;
        setter(&mut config.write(), value);
        Ok(())
    }

    fn update_optional_usize(
        config: &GlobalConfig,
        value: &str,
        setter: impl Fn(&mut Config, Option<usize>),
    ) -> Result<()> {
        let value = parse_value(value)?;
        setter(&mut config.write(), value);
        Ok(())
    }

    fn update_optional_isize(
        config: &GlobalConfig,
        value: &str,
        setter: impl Fn(&mut Config, Option<isize>),
    ) -> Result<()> {
        let value = parse_value(value)?;
        setter(&mut config.write(), value);
        Ok(())
    }

    fn update_optional_bool(
        config: &GlobalConfig,
        value: &str,
        setter: impl Fn(&mut Config, Option<bool>),
    ) -> Result<()> {
        let value = parse_value(value)?;
        setter(&mut config.write(), value);
        Ok(())
    }

    fn update_optional_string(
        config: &GlobalConfig,
        value: &str,
        setter: impl Fn(&mut Config, Option<String>),
    ) -> Result<()> {
        let value = parse_value(value)?;
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
        config.write().set_model_fallbacks(value);
        Ok(())
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

    pub fn set_save_session(&mut self, value: Option<bool>) {
        if let Some(session) = self.session.as_mut() {
            session.set_save_session(value);
        } else {
            self.save_session = value;
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
