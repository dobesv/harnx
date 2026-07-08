//! MCP/ACP server management extracted from config/mod.rs for code health.
use super::*;
use harnx_acp::AcpServerConfig;
use harnx_core::package_namespace::qualify_agent_name;
use harnx_mcp::McpServerConfig;
use std::env;

fn normalize_package_acp_server_args(server: &mut AcpServerConfig, pkg_name: &str) {
    let stem = server.name.as_str();
    let qualified = qualify_agent_name(pkg_name, stem);
    for arg in &mut server.args {
        if arg == stem {
            *arg = qualified.clone();
        }
    }
}

fn effective_mcp_servers(
    mcp_servers: &[McpServerConfig],
    mcp_root: &[String],
    agent_package: Option<&str>,
) -> Vec<McpServerConfig> {
    let mut effective_servers: Vec<McpServerConfig> = mcp_servers
        .iter()
        .filter_map(|server| {
            if !server.enabled {
                return None;
            }
            let mut server = server.clone();
            server.name = mcp_server_display_name(&server, agent_package);
            Some(server)
        })
        .collect();

    let mut extra_roots = mcp_root.to_vec();
    if let Ok(cwd) = env::current_dir() {
        #[cfg(unix)]
        if path_is_home_or_ancestor(&cwd) {
            warn!(
                "sandbox: skipping CWD {:?} as MCP root — equals or is ancestor of $HOME",
                cwd.display()
            );
        } else if let Ok(cwd_str) = cwd.into_os_string().into_string() {
            if !extra_roots.contains(&cwd_str) {
                extra_roots.insert(0, cwd_str);
            }
        }
        #[cfg(not(unix))]
        if let Ok(cwd_str) = cwd.into_os_string().into_string() {
            if !extra_roots.contains(&cwd_str) {
                extra_roots.insert(0, cwd_str);
            }
        }
    }
    if !extra_roots.is_empty() {
        for server in &mut effective_servers {
            for root in extra_roots.iter().rev() {
                #[cfg(unix)]
                if path_is_home_or_ancestor(Path::new(root)) {
                    warn!(
                        "sandbox: skipping root {:?} from mcp_roots — equals or is ancestor of $HOME",
                        root
                    );
                    continue;
                }
                if !server.roots.contains(root) {
                    server.roots.insert(0, root.clone());
                }
            }
        }
    }

    // Sort by name so this matches the ordering of `McpManager::configs()`,
    // which the manager-reuse check in `reinit_managers_for_agent` compares
    // against. Without this, servers defined out of alphabetical order in YAML
    // would compare unequal and defeat the #988 no-churn preservation.
    effective_servers.sort_by(|left, right| left.name.cmp(&right.name));

    effective_servers
}

fn effective_acp_servers(
    acp_servers: &[AcpServerConfig],
    agent_package: Option<&str>,
) -> Vec<AcpServerConfig> {
    let mut effective_servers: Vec<AcpServerConfig> = acp_servers
        .iter()
        .map(|server| {
            let mut server = server.clone();
            server.name = acp_server_display_name(&server, agent_package);
            server
        })
        .collect();

    // Sort by name to match `AcpManager::configs()` ordering (see the MCP note
    // above) so the manager-reuse comparison is order-insensitive.
    effective_servers.sort_by(|left, right| left.name.cmp(&right.name));

    effective_servers
}

impl Config {
    /// Load MCP and ACP servers from a single package directory.
    ///
    /// Servers are stored with their bare names (the yaml stem) and tagged with
    /// `package = Some(pkg_name)`.  The actual display name used with the LLM
    /// — bare or prefixed — is decided at `init_mcp_manager_for_agent` time
    /// based on which agent is active.
    pub(super) fn load_package_servers(config: &mut Config, pkg_path: &Path, pkg_name: &str) {
        // Load the patch file once — shared by MCP servers and clients.
        let patch = load_package_mcp_patch(pkg_name);

        let pkg_mcp_dir = pkg_path.join(paths::MCP_SERVERS_DIR_NAME);
        if pkg_mcp_dir.is_dir() {
            for mut server in Self::load_mcp_servers_from_dir(&pkg_mcp_dir).unwrap_or_default() {
                server.package = Some(pkg_name.to_string());
                if let Some(patch) = &patch {
                    if let Err(e) = apply_mcp_server_patch(&mut server, &patch.mcp_servers) {
                        log::error!(
                            "Package patch failed for MCP server '{}': {e:#}",
                            server.name
                        );
                        continue;
                    }
                }
                config.mcp_servers.push(server);
            }
        }

        let pkg_acp_dir = pkg_path.join(paths::ACP_SERVERS_DIR_NAME);
        if pkg_acp_dir.is_dir() {
            for mut server in Self::load_acp_servers_from_dir(&pkg_acp_dir).unwrap_or_default() {
                server.package = Some(pkg_name.to_string());
                normalize_package_acp_server_args(&mut server, pkg_name);
                config.acp_servers.push(server);
            }
        }

        config.clients.extend(Self::load_package_clients(
            pkg_path,
            pkg_name,
            patch.as_ref(),
        ));
    }

    pub(super) fn load_mcp_servers_from_dir(dir: &Path) -> Result<Vec<McpServerConfig>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut servers = Vec::new();
        for path in Self::sorted_yaml_files(dir)? {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let content = read_to_string(&path).with_context(|| {
                format!("Failed to read MCP server config '{}'", path.display())
            })?;
            let mut server: McpServerConfig =
                serde_yaml::from_str(&content).with_context(|| {
                    format!("Failed to parse MCP server config '{}'", path.display())
                })?;
            server.name = stem;
            servers.push(server);
        }
        Ok(servers)
    }

    pub(super) fn load_acp_servers_from_dir(dir: &Path) -> Result<Vec<AcpServerConfig>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut servers = Vec::new();
        for path in Self::sorted_yaml_files(dir)? {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let content = read_to_string(&path).with_context(|| {
                format!("Failed to read ACP server config '{}'", path.display())
            })?;
            let mut server: AcpServerConfig =
                serde_yaml::from_str(&content).with_context(|| {
                    format!("Failed to parse ACP server config '{}'", path.display())
                })?;
            server.name = stem;
            servers.push(server);
        }
        Ok(servers)
    }

    pub(super) fn needs_mcp_tools(&self) -> bool {
        self.mcp_manager.is_some()
    }

    /// (Re)initialize the MCP and ACP managers for the given active agent.
    ///
    /// `agent_package` is `Some("mypkg")` when the active agent belongs to
    /// an installed package, or `None` for top-level agents.
    ///
    /// Package MCP/ACP servers that belong to the **same** package as the
    /// active agent are registered under their **bare name** (e.g. `fs`),
    /// so the LLM sees tools as `fs_read_file`.  Servers from other packages
    /// keep their prefixed name (e.g. `otherpkg__db`), and top-level servers
    /// are always registered under their original name.
    ///
    /// Re-activating same agent/scope should preserve existing managers so MCP
    /// and ACP subprocesses stay alive across prompts. Only rebuild when the
    /// effective scoped server config changes.
    pub(crate) fn reinit_managers_for_agent(&mut self, agent_package: Option<&str>) {
        let mcp_servers = effective_mcp_servers(&self.mcp_servers, &self.mcp_root, agent_package);
        self.mcp_manager = if mcp_servers.is_empty() {
            None
        } else if let Some(existing) = self.mcp_manager.as_ref() {
            if existing.configs() == mcp_servers {
                Some(existing.clone())
            } else {
                let manager = McpManager::new();
                manager.initialize(mcp_servers);
                Some(Arc::new(manager))
            }
        } else {
            let manager = McpManager::new();
            manager.initialize(mcp_servers);
            Some(Arc::new(manager))
        };

        let acp_servers = effective_acp_servers(&self.acp_servers, agent_package);
        self.acp_manager = if acp_servers.is_empty() {
            None
        } else if let Some(existing) = self.acp_manager.as_ref() {
            if existing.configs() == acp_servers {
                Some(existing.clone())
            } else {
                let manager = AcpManager::new();
                manager.initialize(acp_servers);
                Some(Arc::new(manager))
            }
        } else {
            let manager = AcpManager::new();
            manager.initialize(acp_servers);
            Some(Arc::new(manager))
        };
    }

    pub fn init_mcp_manager(&mut self) {
        self.reinit_managers_for_agent(None);
    }

    pub(super) fn init_acp_manager(&mut self) {
        // ACP init is folded into reinit_managers_for_agent; this stub exists
        // so the call-site in Config::init() continues to compile unchanged.
    }

    pub fn mcp_list_servers(config: &GlobalConfig) -> Vec<String> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => manager.list_servers(),
            None => vec![],
        }
    }

    pub(super) fn mcp_list_servers_from_config(&self) -> Vec<String> {
        match &self.mcp_manager {
            Some(manager) => manager.list_servers(),
            None => vec![],
        }
    }

    pub async fn mcp_connect_server(config: &GlobalConfig, server_name: &str) -> Result<()> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => manager.connect(server_name).await,
            None => bail!("MCP is not configured"),
        }
    }

    pub async fn mcp_disconnect_server(config: &GlobalConfig, server_name: &str) -> Result<()> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => manager.disconnect(server_name).await,
            None => bail!("MCP is not configured"),
        }
    }

    pub fn mcp_get_roots(config: &GlobalConfig, server_name: &str) -> Result<Vec<String>> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => {
                let client = manager
                    .get_client(server_name)
                    .ok_or_else(|| anyhow!("MCP server '{}' not found", server_name))?;
                Ok(client.get_roots())
            }
            None => bail!("MCP is not configured"),
        }
    }

    pub async fn mcp_add_root(config: &GlobalConfig, server_name: &str, root: &str) -> Result<()> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => {
                let client = manager
                    .get_client(server_name)
                    .ok_or_else(|| anyhow!("MCP server '{}' not found", server_name))?;
                client.add_root(root).await
            }
            None => bail!("MCP is not configured"),
        }
    }

    pub async fn mcp_remove_root(
        config: &GlobalConfig,
        server_name: &str,
        root: &str,
    ) -> Result<()> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => {
                let client = manager
                    .get_client(server_name)
                    .ok_or_else(|| anyhow!("MCP server '{}' not found", server_name))?;
                client.remove_root(root).await
            }
            None => bail!("MCP is not configured"),
        }
    }
}
