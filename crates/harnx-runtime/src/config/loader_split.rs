//! Config loading/initialization extracted from config/mod.rs for code health.
use super::*;
use harnx_core::agent_config::AgentConfig;

fn normalize_description(description: Option<String>) -> Option<String> {
    description.and_then(|description| {
        let description = description.trim().to_string();
        (!description.is_empty()).then_some(description)
    })
}

impl Config {
    pub async fn init(
        working_mode: WorkingMode,
        info_flag: bool,
        mut mcp_root: Vec<String>,
    ) -> Result<Self> {
        // Install any user-supplied models-override list before the
        // harnx-client `ALL_PROVIDER_MODELS` lazy-lock is first accessed.
        crate::client::install_models_override();

        let config_path = Self::config_file();
        let mut config = if !config_path.exists() {
            match env::var(get_env_name("provider"))
                .ok()
                .or_else(|| env::var(get_env_name("platform")).ok())
            {
                Some(v) => Self::load_dynamic(&v)?,
                None => {
                    if *IS_STDOUT_TERMINAL {
                        create_config_file(&config_path).await?;
                    }
                    Self::load_from_file(&config_path)?
                }
            }
        } else {
            Self::load_from_file(&config_path)?
        };

        if let Ok(v) = env::var("HARNX_MCP_ROOTS") {
            for root in v.split(',') {
                let root = root.trim();
                if !root.is_empty() && !mcp_root.contains(&root.to_string()) {
                    mcp_root.push(root.to_string());
                }
            }
        }

        config.working_mode = working_mode;
        config.info_flag = info_flag;
        config.mcp_root = mcp_root;

        let setup = |config: &mut Self| -> Result<()> {
            config.load_envs();

            if let Some(wrap) = config.wrap.clone() {
                config.set_wrap(&wrap)?;
            }

            config.init_mcp_manager();
            config.init_acp_manager();
            config.tools = Tools::init_from_mcp(None);

            config.setup_model()?;
            config.setup_document_loaders();
            config.setup_user_agent();
            Ok(())
        };
        let ret = setup(&mut config);
        if !info_flag {
            ret?;
        }
        Ok(config)
    }

    pub(crate) fn load_from_file(config_path: &Path) -> Result<Self> {
        let err = || format!("Failed to load config at '{}'", config_path.display());
        let data: ConfigData = if config_path.exists() {
            let content = read_to_string(config_path).with_context(err)?;
            serde_yaml::from_str(&content)
                .map_err(|err| anyhow!(err.to_string()))
                .with_context(err)?
        } else {
            ConfigData::default()
        };
        let mut config = Self {
            show_sequence_numbers: data.show_sequence_numbers,
            show_timestamps: data.show_timestamps,
            data,
            ..Self::default()
        };
        let config_dir = config_path.parent().unwrap_or(config_path);
        config.clients = Self::load_clients_from_dir(&config_dir.join(paths::CLIENTS_DIR_NAME))?;
        config.nats_servers =
            Self::load_nats_servers_from_dir(&config_dir.join(paths::NATS_SERVERS_DIR_NAME))?;
        config.mcp_servers =
            Self::load_mcp_servers_from_dir(&config_dir.join(paths::MCP_SERVERS_DIR_NAME))?;
        config.acp_servers =
            Self::load_acp_servers_from_dir(&config_dir.join(paths::ACP_SERVERS_DIR_NAME))?;
        let packages_dir = paths::packages_dir();
        if packages_dir.is_dir() {
            Self::load_packages(&mut config, &packages_dir)?;
        }
        Self::auto_register_agents(&mut config.acp_servers)?;

        Ok(config)
    }

    fn load_packages(config: &mut Config, packages_dir: &Path) -> Result<()> {
        let Ok(entries) = std::fs::read_dir(packages_dir) else {
            return Ok(());
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let Some(pkg_name) = package_dir_name(&path) else {
                continue;
            };
            Self::load_package_servers(config, &path, &pkg_name);
        }
        Ok(())
    }

    /// Load clients from a single package directory, applying any patch expressions.
    pub(super) fn load_package_clients(
        pkg_path: &Path,
        pkg_name: &str,
        patch: Option<&harnx_core::package::PackagePatch>,
    ) -> Vec<ClientConfig> {
        let pkg_clients_dir = pkg_path.join(paths::CLIENTS_DIR_NAME);
        if !pkg_clients_dir.is_dir() {
            return vec![];
        }
        let mut clients = Self::load_clients_from_dir(&pkg_clients_dir).unwrap_or_default();
        if let Some(patch) = patch {
            clients.retain_mut(|client| match apply_client_patch(client, &patch.clients) {
                Ok(()) => true,
                Err(e) => {
                    log::error!(
                        "Package patch failed for client '{}': {e:#}",
                        client.effective_name()
                    );
                    false
                }
            });
        }
        // Qualify client names with package prefix after patching.
        // Must be after patching because apply_client_patch round-trips through
        // serde_json and resets #[serde(skip)] fields like `package`.
        // Always restore `package` even for explicitly-named clients whose
        // name already contains '/'.
        for client in &mut clients {
            let resolved_name = harnx_core::package_namespace::resolve_package_relative_name(
                client.effective_name(),
                Some(pkg_name),
            );
            client.set_name(resolved_name);
            client.set_package(Some(pkg_name.to_string()));
        }
        clients
    }

    fn load_clients_from_dir(dir: &Path) -> Result<Vec<ClientConfig>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut clients = Vec::new();
        for path in Self::sorted_yaml_files(dir)? {
            // Derive the client name from the filename stem. Skip paths with a
            // missing or empty stem (e.g. non-UTF-8 names) — an empty name would
            // violate ClientConfig::set_name's debug_assert.
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(stem) if !stem.is_empty() => stem.to_string(),
                _ => {
                    log::warn!(
                        "Skipping client config with invalid filename stem: '{}'",
                        path.display()
                    );
                    continue;
                }
            };
            let content = read_to_string(&path)
                .with_context(|| format!("Failed to read client config '{}'", path.display()))?;
            let mut client: ClientConfig = serde_yaml::from_str(&content)
                .with_context(|| format!("Failed to parse client config '{}'", path.display()))?;
            client.set_name(stem);
            clients.push(client);
        }
        Ok(clients)
    }

    fn auto_register_agents(acp_servers: &mut Vec<AcpServerConfig>) -> Result<()> {
        let existing_names: HashSet<String> = acp_servers
            .iter()
            .map(|server| server.name.clone())
            .collect();
        let remote_descriptions = Self::remote_agent_description_map();
        let local_descriptions = Self::local_agent_description_map(list_agents());
        let command = harnx_acp_server_command();
        for agent_name in list_agents() {
            if !existing_names.contains(&agent_name) {
                // Extract the package for package agents (e.g. "mypkg/coder" → Some("mypkg")).
                // Top-level agents have package = None.
                let pkg = harnx_core::package_namespace::pkg_from_qualified(&agent_name)
                    .map(str::to_string);
                let description = remote_descriptions
                    .get(&agent_name)
                    .cloned()
                    .flatten()
                    .or_else(|| local_descriptions.get(&agent_name).cloned().flatten());
                acp_servers.push(AcpServerConfig {
                    name: agent_name.clone(),
                    command: command.clone(),
                    args: vec![agent_name.clone()],
                    env: Default::default(),
                    enabled: true,
                    description,
                    idle_timeout_secs: 300,
                    operation_timeout_secs: 3600,
                    package: pkg,
                });
            }
        }
        Ok(())
    }

    pub(super) fn remote_agent_description_map() -> HashMap<String, Option<String>> {
        let nats_servers_dir = Self::config_dir().join(paths::NATS_SERVERS_DIR_NAME);
        let Ok(servers) = Self::load_nats_servers_from_dir(&nats_servers_dir) else {
            return HashMap::new();
        };

        servers
            .into_iter()
            .flat_map(|server| {
                let cluster_name = server.name;
                server.agents.into_iter().map(move |agent| {
                    (
                        format!("{}@{}", agent.name, cluster_name),
                        normalize_description(agent.description),
                    )
                })
            })
            .collect()
    }

    pub(super) fn local_agent_description_map<I>(agent_names: I) -> HashMap<String, Option<String>>
    where
        I: IntoIterator<Item = String>,
    {
        agent_names
            .into_iter()
            .filter(|agent_name| !agent_name.contains('@'))
            .map(|agent_name| {
                let description = std::fs::read_to_string(Self::agent_file(&agent_name))
                    .ok()
                    .and_then(|content| AgentConfig::from_markdown(&agent_name, &content).ok())
                    .map(|config| normalize_description(Some(config.description().to_string())))
                    .unwrap_or(None);
                (agent_name, description)
            })
            .collect()
    }

    pub(super) fn sorted_yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
        let entries = read_dir(dir)
            .with_context(|| format!("Failed to read directory '{}'", dir.display()))?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry =
                entry.with_context(|| format!("Failed to read entry in '{}'", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }

    fn load_dynamic(model_id: &str) -> Result<Self> {
        let provider = match model_id.split_once(':') {
            Some((v, _)) => v,
            _ => model_id,
        };
        let is_openai_compatible = OPENAI_COMPATIBLE_PROVIDERS
            .into_iter()
            .any(|(name, _)| provider == name);
        let client = if is_openai_compatible {
            json!({ "type": "openai-compatible" })
        } else {
            json!({ "type": provider })
        };
        let data_value = json!({
            "model": model_id.to_string(),
            "save": false,
        });
        let data: ConfigData =
            serde_json::from_value(data_value).with_context(|| "Failed to load config from env")?;

        let mut config = Self {
            data,
            ..Self::default()
        };

        let mut client: ClientConfig =
            serde_json::from_value(client).context("Failed to parse client config")?;
        client.set_name(provider.to_string());
        config.clients = vec![client];

        let config_dir = Self::config_dir();
        config.nats_servers =
            Self::load_nats_servers_from_dir(&config_dir.join(paths::NATS_SERVERS_DIR_NAME))?;
        config.mcp_servers =
            Self::load_mcp_servers_from_dir(&config_dir.join(paths::MCP_SERVERS_DIR_NAME))?;
        config.acp_servers =
            Self::load_acp_servers_from_dir(&config_dir.join(paths::ACP_SERVERS_DIR_NAME))?;
        Self::auto_register_agents(&mut config.acp_servers)?;
        Ok(config)
    }

    fn setup_model(&mut self) -> Result<()> {
        let mut model_id = self.model_id.clone();
        if model_id.is_empty() {
            let models = list_models(&self.clients, ModelType::Chat);
            if models.is_empty() {
                bail!("No available model");
            }
            model_id = models[0].id()
        };
        self.set_model(&model_id)?;
        self.model_id = model_id;
        Ok(())
    }

    fn setup_document_loaders(&mut self) {
        [("pdf", "pdftotext $1 -"), ("docx", "pandoc --to plain $1")]
            .into_iter()
            .for_each(|(k, v)| {
                let (k, v) = (k.to_string(), v.to_string());
                self.document_loaders.entry(k).or_insert(v);
            });
    }

    fn setup_user_agent(&mut self) {
        if let Some("auto") = self.user_agent.as_deref() {
            self.user_agent = Some(format!(
                "{}/{}",
                env!("CARGO_CRATE_NAME"),
                env!("CARGO_PKG_VERSION")
            ));
        }
    }
}

fn harnx_acp_server_command() -> String {
    std::env::current_exe()
        .map(|current_exe| harnx_acp_server_command_from_current_exe(&current_exe))
        .unwrap_or_else(|_| fallback_harnx_acp_server_command())
}

fn harnx_acp_server_command_from_current_exe(current_exe: &Path) -> String {
    harnx_acp_server_command_from_parent(current_exe.parent(), current_exe.is_absolute())
}

fn harnx_acp_server_command_from_parent(
    parent_dir: Option<&Path>,
    current_exe_is_absolute: bool,
) -> String {
    let Some(parent_dir) = parent_dir else {
        return fallback_harnx_acp_server_command();
    };
    if !current_exe_is_absolute {
        return fallback_harnx_acp_server_command();
    }
    let sibling = parent_dir.join(harnx_acp_server_binary_name());
    if sibling.is_file() {
        sibling.to_string_lossy().to_string()
    } else {
        fallback_harnx_acp_server_command()
    }
}

#[cfg(windows)]
fn harnx_acp_server_binary_name() -> &'static str {
    "harnx-acp-server.exe"
}

#[cfg(not(windows))]
fn harnx_acp_server_binary_name() -> &'static str {
    "harnx-acp-server"
}

fn fallback_harnx_acp_server_command() -> String {
    String::from("harnx-acp-server")
}

async fn create_config_file(config_path: &Path) -> Result<()> {
    let ans = Confirm::new("No config file, create a new one?")
        .with_default(true)
        .prompt()?;
    if !ans {
        process::exit(0);
    }

    let client = Select::new("API Provider (required):", list_client_types()).prompt()?;

    let (model, clients_config) = create_client_config(client).await?;
    let config = serde_json::json!({ "model": model });
    let config_data = serde_yaml::to_string(&config).with_context(|| "Failed to create config")?;
    let config_data =
        format!("# see https://github.com/dobesv/harnx/blob/main/example_config\n\n{config_data}");

    ensure_parent_exists_async(config_path).await?;
    tokio::fs::write(config_path, config_data)
        .await
        .with_context(|| format!("Failed to write to '{}'", config_path.display()))?;

    let clients_dir = config_path
        .parent()
        .unwrap_or(config_path)
        .join(paths::CLIENTS_DIR_NAME);
    tokio::fs::create_dir_all(&clients_dir)
        .await
        .with_context(|| format!("Failed to create '{}'", clients_dir.display()))?;
    let client_filename = clients_config
        .get("name")
        .or_else(|| clients_config.get("type"))
        .and_then(|value| value.as_str())
        .unwrap_or("default");
    let client_path = clients_dir.join(format!("{client_filename}.yaml"));
    let client_data =
        serde_yaml::to_string(&clients_config).with_context(|| "Failed to create client config")?;
    tokio::fs::write(&client_path, client_data)
        .await
        .with_context(|| format!("Failed to write to '{}'", client_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::prelude::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(config_path, perms.clone()).await?;
        tokio::fs::set_permissions(&client_path, perms).await?;
    }

    crate::utils::emit_info(format!(
        "✓ Saved the config file to '{}'.",
        config_path.display()
    ));
    crate::utils::emit_info(format!(
        "✓ Saved the client config to '{}'.",
        client_path.display()
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::harnx_acp_server_command_from_current_exe;
    use std::{fs, path::PathBuf};

    #[test]
    fn harnx_acp_server_command_uses_existing_sibling_binary() {
        let temp_dir = unique_temp_dir("harnx-acp-server-sibling-present");
        fs::create_dir_all(&temp_dir).unwrap();

        let current_exe = temp_dir.join(current_exe_name());
        fs::write(&current_exe, b"").unwrap();

        let sibling = temp_dir.join(harnx_acp_server_binary_name());
        fs::write(&sibling, b"").unwrap();

        assert_eq!(
            harnx_acp_server_command_from_current_exe(&current_exe),
            sibling.to_string_lossy().to_string()
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn harnx_acp_server_command_falls_back_when_sibling_missing() {
        let temp_dir = unique_temp_dir("harnx-acp-server-sibling-missing");
        fs::create_dir_all(&temp_dir).unwrap();

        let current_exe = temp_dir.join(current_exe_name());
        fs::write(&current_exe, b"").unwrap();

        assert_eq!(
            harnx_acp_server_command_from_current_exe(&current_exe),
            "harnx-acp-server"
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    #[cfg(windows)]
    fn current_exe_name() -> &'static str {
        "harnx-runtime-test.exe"
    }

    #[cfg(not(windows))]
    fn current_exe_name() -> &'static str {
        "harnx-runtime-test"
    }

    #[cfg(windows)]
    fn harnx_acp_server_binary_name() -> &'static str {
        "harnx-acp-server.exe"
    }

    #[cfg(not(windows))]
    fn harnx_acp_server_binary_name() -> &'static str {
        "harnx-acp-server"
    }
}
