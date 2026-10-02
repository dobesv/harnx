//! Macro management extracted from config/mod.rs for code health.
use super::*;

impl Config {
    pub fn list_macros() -> Vec<String> {
        list_file_names(Self::macros_dir(), ".yaml")
    }

    pub fn load_macro(name: &str) -> Result<Macro> {
        let path = Self::macro_file(name);
        let err = || format!("Failed to load macro '{name}' at '{}'", path.display());
        let content = read_to_string(&path).with_context(err)?;
        let value: Macro = serde_yaml::from_str(&content).with_context(err)?;
        Ok(value)
    }

    pub fn has_macro(name: &str) -> bool {
        let names = Self::list_macros();
        names.contains(&name.to_string())
    }

    pub fn new_macro(&mut self, name: &str) -> Result<()> {
        if self.macro_flag {
            bail!("No macro");
        }
        let ans = Confirm::new("Create a new macro?")
            .with_default(true)
            .prompt()?;
        if ans {
            let macro_path = Self::macro_file(name);
            ensure_parent_exists(&macro_path)?;
            self.edit_with_tui_hooks(|this| {
                let editor = this.editor()?;
                edit_file(&editor, &macro_path)
            })?;
        } else {
            bail!("No macro");
        }
        Ok(())
    }
}

#[async_recursion::async_recursion]
pub async fn macro_execute(
    config: &GlobalConfig,
    name: &str,
    args: Option<&str>,
    abort_signal: AbortSignal,
) -> Result<()> {
    let macro_value = Config::load_macro(name)?;
    let (mut new_args, text) = split_args_text(args.unwrap_or_default(), cfg!(windows));
    if !text.is_empty() {
        new_args.push(text.to_string());
    }
    let variables = macro_value
        .resolve_variables(&new_args)
        .map_err(|err| anyhow!("{err}. Usage: {}", macro_value.usage(name)))?;
    let enclosing_macro = config.read().macro_flag;
    let parent_config = config;
    let agent = config.read().extract_agent();
    let mut config = config.read().clone();
    config.temperature = agent.temperature();
    config.top_p = agent.top_p();
    config.use_tools = agent.use_tools();
    config.macro_flag = true;
    config.model = agent.model().clone();
    config.session = None;
    config.rag = None;
    config.agent = None;
    config.discontinuous_last_message();
    let config = Arc::new(crate::config::ConfigLock::new(config));
    config.write().macro_flag = true;
    // A finite outer macro scope starts before any step, including the 24-hour default. It is persisted
    // independently of editable session text and copied into every model step.
    if config.read().run_context.is_none() {
        let now = chrono::Utc::now();
        let snapshot = config.read().clone();
        let record = crate::nats_session_metadata::RunLimitsRecord::admit_root(
            Default::default(),
            Default::default(),
            now,
            snapshot.data.run_limits,
            None,
            crate::nats_session_metadata::CallTimeoutOverride::Omitted,
        )?;
        let cluster = match &snapshot.nats_routing {
            crate::config::NatsRouting::Cluster(name) => name.as_str(),
            _ => LOCAL_CLUSTER_KEY,
        };
        let js = snapshot.nats_jetstream(cluster).await?;
        let replicas = snapshot
            .resolve_nats_server(cluster)
            .await?
            .resolved_replicas();
        let store =
            crate::nats_session_metadata::SessionMetadataStore::ensure(&js, replicas).await?;
        let storage = snapshot
            .session
            .as_ref()
            .map(|session| session.storage_key())
            .unwrap_or_else(|| {
                harnx_core::session_identity::session_key(
                    None,
                    &format!("macro-{}", record.run_id.as_str()),
                )
            });
        store.put_run_limits(&storage, &record).await?;
        store.put_invocation_limits(&storage, &record).await?;
        config.write().run_context = Some(record);
    }

    for step in &macro_value.steps {
        let record = config.read().run_context.clone();
        // Tokio's timeout polls a ready command even with zero remaining time.
        // Refuse expired work before any dot-command side effects can run.
        if record
            .as_ref()
            .is_some_and(|record| record.is_expired_at(chrono::Utc::now()))
        {
            abort_signal.set_ctrlc();
            bail!("autonomous macro deadline expired; external side effects may be unknown");
        }
        let command = Macro::interpolate_command(step, &variables);
        crate::utils::emit_info(format!(">> {}", multiline_text(&command)));
        let remaining = record
            .as_ref()
            .and_then(|record| record.deadline)
            .map(|deadline| {
                deadline
                    .signed_duration_since(chrono::Utc::now())
                    .to_std()
                    .unwrap_or_default()
            });
        match remaining {
            Some(remaining) => {
                match tokio::time::timeout(
                    remaining,
                    run_command(&config, abort_signal.clone(), &command),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        abort_signal.set_ctrlc();
                        bail!("autonomous macro deadline expired; external side effects may be unknown");
                    }
                }
            }
            None => run_command(&config, abort_signal.clone(), &command).await?,
        };
    }
    if enclosing_macro {
        // Keep agent/session settings isolated, but carry the last invocation's
        // frozen scope forward so a shorter nested allowance cannot be escaped.
        parent_config.write().run_context = config.read().run_context.clone();
    }
    Ok(())
}
