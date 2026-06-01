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
    let config = Arc::new(RwLock::new(config));
    config.write().macro_flag = true;
    let mut async_manager = AsyncHookManager::new();
    let persistent_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
        harnx_hooks::PersistentHookManager::new(),
    ));
    let mut pending_async_context = None;
    for step in &macro_value.steps {
        let command = Macro::interpolate_command(step, &variables);
        crate::utils::emit_info(format!(">> {}", multiline_text(&command)));
        run_command(
            &config,
            abort_signal.clone(),
            &command,
            &mut async_manager,
            &persistent_manager,
            &mut pending_async_context,
        )
        .await?;
    }
    persistent_manager.lock().await.shutdown();
    Ok(())
}
