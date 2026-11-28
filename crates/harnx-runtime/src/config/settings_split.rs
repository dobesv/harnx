//! Reserved for config/mod.rs cluster extraction (code health). Currently unused.
use super::{update_rag, Agent, Config, GlobalConfig, Session};
use crate::client::ModelType;
use crate::nats_session_metadata::SessionOverrideUpdate;
use anyhow::{anyhow, bail, Context, Result};

/// A tool-selector setting's `.set` behavior, bundled so
/// [`Config::update_tool_selectors`] keeps a short argument list: how a raw
/// value parses (with the setting's own meaning for `null`), which session
/// override to persist, and how the parsed value lands in memory.
struct ToolSelectorSetting {
    parse: fn(&str) -> Option<Vec<String>>,
    persist: fn(Option<Vec<String>>) -> SessionOverrideUpdate,
    apply: fn(&mut Config, Option<Vec<String>>) -> Result<()>,
}

/// `use_tools` as a `.set` target: `null` clears the override, so the
/// agent's selectors apply again; with no session or agent the value falls
/// back to the global default.
const USE_TOOLS: ToolSelectorSetting = ToolSelectorSetting {
    parse: |value| (value != "null").then(|| Config::split_selectors(value)),
    persist: SessionOverrideUpdate::UseTools,
    apply: |config, value| {
        config.set_use_tools(value);
        Ok(())
    },
};

/// `bash_tools` as a `.set` target. Unlike `use_tools`, `null` is an
/// explicit clear, not an unset override: the clear persists as an empty
/// selector list so worker reconstruction re-applies it instead of falling
/// back to the agent file's `bash_tools`. `SessionOverrides` uses `Option`,
/// where `None` means "no override" (and serializes as absent).
const BASH_TOOLS: ToolSelectorSetting = ToolSelectorSetting {
    parse: |value| {
        Some(if value == "null" {
            Vec::new()
        } else {
            Config::split_selectors(value)
        })
    },
    persist: SessionOverrideUpdate::BashTools,
    apply: Config::set_bash_tools,
};

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
    ///
    /// This sync path needs the session's persistence sink (installed by the
    /// first local turn); without one it fails rather than update memory and
    /// claim success with no durable write. The destination is captured once
    /// (`session::capture_title_target`) and used once, so a `.session`
    /// switch racing the write can neither redirect it nor land the memory
    /// update or event on a replacement session.
    // qual:allow(iosp) reason: "the sink check and post-write result guard prevent a title from being reported successful unless persistence succeeded"
    fn set_session_title(config: &GlobalConfig, value: &str) -> Result<()> {
        let (target, record) = Self::capture_manual_title_write(config, value)?;
        let title = record.title.clone();
        if !target.has_sink() {
            bail!(concat!(
                "Cannot durably set the title on this session yet: it has no active ",
                "broker connection (before the first local turn). Try again once the ",
                "broker is reachable."
            ));
        }
        if !crate::config::session::record_title_captured(config, target, record)? {
            bail!("The active session changed before its title was set");
        }
        harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
            harnx_core::event::SessionEvent::TitleUpdated(title),
        ));
        Ok(())
    }

    /// Capture coherent manual-title inputs and the full active destination
    /// under a single config read. Returns `Usage: .set title <text>` on empty
    /// text and `No active session to set a title on` when no session is
    /// selected.
    pub fn capture_manual_title_write(
        config: &GlobalConfig,
        value: &str,
    ) -> Result<(
        crate::config::session::TitleWriteTarget,
        crate::config::session::TitleRecord,
    )> {
        let title = value.trim().to_string();
        if title.is_empty() {
            bail!("Usage: .set title <text>");
        }
        let (target, tokens) =
            crate::config::session_persistence::capture_active_title_target(config)
                .context("No active session to set a title on")?;
        Ok((
            target,
            crate::config::session::TitleRecord {
                title,
                manual: true,
                tokens,
            },
        ))
    }

    /// Parse a `.set title [<text>]` line using the exact `.set` command token
    /// boundary (`^\s*(\.\S*)\s*` matching `".set"`) and [`durable_title_value`].
    /// Returns `None` for `.settitle`, `/set title`, or other `.set <key>` forms.
    pub fn parse_dot_set_title(line: &str) -> Option<&str> {
        line.trim_start()
            .strip_prefix(".set")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            .map(str::trim)
            .and_then(Self::durable_title_value)
    }

    fn durable_title_value(data: &str) -> Option<&str> {
        let rest = data.trim_start().strip_prefix("title")?;
        (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest.trim())
    }

    /// `.set` dispatch that gives explicit writes their durable path.
    ///
    /// `title` is the one setting whose write must reach the broker even
    /// before the first local turn installs a session sink; it runs
    /// [`session::record_title_durably`] with the caller's cancellation. Every
    /// other key keeps the sync [`Config::update`] path unchanged.
    // qual:allow(iosp) reason: "the single dispatch condition selects the durable broker write only for title; all other settings retain the synchronous update path"
    pub async fn update_setting_durably(
        config: &GlobalConfig,
        data: &str,
        abort: Option<&crate::utils::AbortSignal>,
    ) -> Result<()> {
        // `title` takes a free-form, possibly multi-word value: mirror
        // `Config::update`'s parsing so both entries accept the same input.
        if let Some(title) = Self::durable_title_value(data) {
            return Self::set_session_title_durably(config, title, abort).await;
        }
        Config::update(config, data)
    }

    /// Same contract as [`Self::set_session_title`], but obtains durable
    /// persistence itself when no sink is installed: cancellable broker
    /// acquisition, fenced on the captured session target, then the same
    /// in-memory update and `TitleUpdated` emission. The full destination
    /// (storage key, cluster, routing, exact optional sink) is captured once
    /// by [`Self::capture_manual_title_write`]; completion — the in-memory title
    /// and the event — is fenced on that captured target, so a `.session`
    /// switch during the write can neither redirect persistence to another
    /// session nor deliver the result to one.
    // qual:allow(iosp) reason: "durable broker result and active-session fence must both pass before reporting success or emitting TitleUpdated"
    async fn set_session_title_durably(
        config: &GlobalConfig,
        value: &str,
        abort: Option<&crate::utils::AbortSignal>,
    ) -> Result<()> {
        let (target, record) = Self::capture_manual_title_write(config, value)?;
        let title = record.title.clone();
        if !crate::config::session::record_title_durably(config, target, record, abort).await? {
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
    /// with the setting's `null` mapping, persist the session override, then
    /// apply it in memory. The per-setting behavior lives in
    /// [`ToolSelectorSetting`], which keeps this function's argument list
    /// short.
    fn update_tool_selectors(
        config: &GlobalConfig,
        value: &str,
        setting: ToolSelectorSetting,
    ) -> Result<()> {
        let value = (setting.parse)(value);
        Self::persist_override(config, (setting.persist)(value.clone()))?;
        (setting.apply)(&mut config.write(), value)
    }

    fn update_use_tools(config: &GlobalConfig, value: &str) -> Result<()> {
        Self::update_tool_selectors(config, value, USE_TOOLS)
    }

    /// Unlike `use_tools`, `.set bash_tools null` is an explicit clear, not an
    /// unset override: see [`BASH_TOOLS`].
    fn update_bash_tools(config: &GlobalConfig, value: &str) -> Result<()> {
        Self::update_tool_selectors(config, value, BASH_TOOLS)
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn config_with_session(_dir: &std::path::Path) -> GlobalConfig {
        let mut config = Config::default();
        let mut session = crate::config::session::new(&config, "title-test", None).unwrap();
        crate::config::session::attach_memory_log(&mut session);
        config.session = Some(session);
        Arc::new(ConfigLock::new(config))
    }

    fn config_with_session_no_sink(_dir: &std::path::Path) -> GlobalConfig {
        let mut config = Config::default();
        let session = crate::config::session::new(&config, "title-test", None).unwrap();
        config.session = Some(session);
        Arc::new(ConfigLock::new(config))
    }

    /// Counts `persist_title` calls so tests can prove the installed sink
    /// handled a title write instead of the no-sink durable fallback.
    struct CountingTitleSink(Arc<AtomicUsize>);

    impl crate::config::session::SessionAppendSink for CountingTitleSink {
        fn append(&self, _entry: &harnx_core::session::SessionLogEntry) -> anyhow::Result<u64> {
            unreachable!("title tests never append")
        }

        fn persist_title(&self, _title: &str, _manual: bool, _tokens: usize) -> anyhow::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn config_with_counting_sink() -> (GlobalConfig, Arc<AtomicUsize>) {
        let mut config = Config::default();
        let mut session = crate::config::session::new(&config, "title-test", None).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        session.runtime = Some(Arc::new(Arc::new(CountingTitleSink(Arc::clone(&calls)))
            as Arc<dyn crate::config::session::SessionAppendSink>));
        config.session = Some(session);
        (Arc::new(ConfigLock::new(config)), calls)
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

    /// The sync entry has no way to reach the broker; a sink-less session
    /// must fail clearly instead of updating memory and claiming success
    /// with no durable write (final-review B5).
    #[test]
    fn set_title_without_sink_fails_clearly_and_leaves_memory_untouched() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_session_no_sink(tmp.path());

        let error = Config::update(&config, "title No Sink Title")
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("Cannot durably set the title"),
            "unexpected error: {error}"
        );
        assert_eq!(config.read().session.as_ref().unwrap().title(), None);
    }

    /// With a sink installed, the durable dispatch routes through it and
    /// never touches the no-sink broker fallback.
    #[tokio::test]
    async fn set_title_durably_routes_through_installed_sink() {
        let (config, calls) = config_with_counting_sink();

        Config::update_setting_durably(&config, "title Sink Path", None)
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let guard = config.read();
        let session = guard.session.as_ref().unwrap();
        assert_eq!(session.title(), Some("Sink Path"));
        assert_eq!(session.title_last_updated_tokens(), usize::MAX);
    }

    /// Cancellation before the first turn (sink-less session, broker
    /// unreachable) fails the explicit write without any memory update.
    #[tokio::test]
    async fn set_title_durably_cancelled_without_sink_fails_without_memory_update() {
        let config = Config {
            nats_routing: crate::config::NatsRouting::Cluster("dead".into()),
            nats_servers: vec![crate::config::NatsServerConfig {
                user_id: None,
                name: "dead".to_string(),
                url: "nats://127.0.0.1:1".to_string(),
                token: None,
                replicas: None,
                tls: None,
                tls_cert: None,
                tls_key: None,
                tls_ca: None,
                ignore_discovered_servers: None,
                agents: vec![],
            }],
            session: Some(
                crate::config::session::new(
                    &Config {
                        nats_routing: crate::config::NatsRouting::Cluster("dead".into()),
                        ..Default::default()
                    },
                    "title-test",
                    None,
                )
                .unwrap(),
            ),
            ..Default::default()
        };
        let config = Arc::new(ConfigLock::new(config));

        let abort = harnx_core::abort::create_abort_signal();
        abort.set_ctrlc();

        let error = Config::update_setting_durably(&config, "title Cancelled Title", Some(&abort))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("cancelled"), "unexpected error: {error}");
        assert_eq!(config.read().session.as_ref().unwrap().title(), None);
    }

    /// A write captured for a session that is no longer active returns `Ok(false)`
    /// before any broker round trip, and never retitles the replacement session.
    #[tokio::test]
    async fn record_title_durably_for_stale_target_rejects_before_connecting() {
        let config = Config {
            nats_routing: crate::config::NatsRouting::Cluster("dead".into()),
            nats_servers: vec![crate::config::NatsServerConfig {
                user_id: None,
                name: "dead".to_string(),
                url: "nats://127.0.0.1:1".to_string(),
                token: None,
                replicas: None,
                tls: None,
                tls_cert: None,
                tls_key: None,
                tls_ca: None,
                ignore_discovered_servers: None,
                agents: vec![],
            }],
            session: Some(
                crate::config::session::new(
                    &Config {
                        nats_routing: crate::config::NatsRouting::Cluster("dead".into()),
                        ..Default::default()
                    },
                    "session-a",
                    None,
                )
                .unwrap(),
            ),
            ..Default::default()
        };
        let config = Arc::new(ConfigLock::new(config));
        let (target, record) = Config::capture_manual_title_write(&config, "Stale Write").unwrap();

        // Switch active session before durable write begins.
        {
            let mut guard = config.write();
            guard.session = Some(crate::config::session::new(&guard, "session-b", None).unwrap());
        }

        let applied = crate::config::session::record_title_durably(&config, target, record, None)
            .await
            .expect("stale target rejects cleanly without dialing dead broker");

        assert!(!applied);
        assert_eq!(config.read().session.as_ref().unwrap().title(), None);
        assert_eq!(
            config
                .read()
                .session
                .as_ref()
                .unwrap()
                .title_last_updated_tokens(),
            0
        );
    }

    #[test]
    fn parse_dot_set_title_respects_command_boundaries() {
        assert_eq!(
            Config::parse_dot_set_title(".set title My Title"),
            Some("My Title")
        );
        assert_eq!(Config::parse_dot_set_title("  .set\ttitle   "), Some(""));
        assert_eq!(Config::parse_dot_set_title(".settitle Foo"), None);
        assert_eq!(Config::parse_dot_set_title("/set title Foo"), None);
        assert_eq!(Config::parse_dot_set_title(".set dry_run true"), None);
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

    /// A config with an explicit agent (its own `bash_tools`, if any), an
    /// active session, and declarations the selectors can resolve against.
    fn config_with_agent_session_and_tools(agent_bash_tools: Option<Vec<String>>) -> Config {
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
        agent.set_bash_tools(agent_bash_tools);
        config.use_agent_obj(agent).unwrap();
        let mut session = crate::config::session::new(&config, "bash-tools-a", None).unwrap();
        crate::config::session::attach_memory_log(&mut session);
        config.session = Some(session);
        config
    }

    #[test]
    fn set_bash_tools_stays_scoped_to_the_active_session() {
        let config = Arc::new(ConfigLock::new(config_with_agent_session_and_tools(Some(
            vec!["grep_search".to_string()],
        ))));

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
    fn session_bash_tools_reach_the_model_request_input_path() {
        // The model's tool declarations come from `prepare_completion_data`
        // as `select_tools(input.agent())`, with the input agent resolved by
        // `input::from_str` when the caller passes no explicit agent. The
        // base agent declares no `bash_tools` of its own, so a `bash`
        // declaration can only appear if the session's override rides along
        // on the input agent — resolving the base agent instead would leave
        // the model without script mode.
        let config = Arc::new(ConfigLock::new(config_with_agent_session_and_tools(None)));

        Config::update(&config, "bash_tools fs_*").unwrap();

        let input = crate::config::input::from_str(&config, "run a script", None);
        let functions = config.read().select_tools(input.agent());
        let functions = functions.expect("tool declarations for the model request");
        assert!(
            functions
                .iter()
                .any(|declaration| declaration.name == "bash"),
            "the model request must carry the session-backed bash declaration"
        );
        assert_eq!(
            config.read().bash_tool_names(input.agent()),
            vec!["fs_read".to_string()],
            "the bash declaration is built from the session's selectors"
        );
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
