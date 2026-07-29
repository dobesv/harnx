//! MCP/ACP server management extracted from config/mod.rs for code health.
use super::*;
use harnx_core::package_namespace::qualify_agent_name;
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

        let pkg_tool_dir = pkg_path.join(paths::TOOL_SERVERS_DIR_NAME);
        if pkg_tool_dir.is_dir() {
            for mut server in Self::load_tool_servers_from_dir(&pkg_tool_dir).unwrap_or_default() {
                server.package = Some(pkg_name.to_string());
                config.tool_servers.push(server);
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
    /// When switching agents this replaces the old managers entirely, killing
    /// any running MCP server subprocesses, giving a clean slate.
    pub(crate) fn reinit_managers_for_agent(&mut self, agent_package: Option<&str>) {
        // ── MCP ──────────────────────────────────────────────────────────────
        self.mcp_manager = if self.mcp_servers.is_empty() {
            None
        } else {
            let mut mcp_servers: Vec<McpServerConfig> = self
                .mcp_servers
                .iter()
                .filter_map(|s| {
                    if !s.enabled {
                        return None;
                    }
                    let mut s = s.clone();
                    s.name = mcp_server_display_name(&s, agent_package);
                    Some(s)
                })
                .collect();

            // Prepend cwd to roots for every server
            let mut extra_roots = self.mcp_root.clone();
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
                for server in mcp_servers.iter_mut() {
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

            let manager = McpManager::new();
            manager.initialize(mcp_servers);
            Some(Arc::new(manager))
        };

        // ── ACP ──────────────────────────────────────────────────────────────
        self.acp_manager = if self.acp_servers.is_empty() {
            None
        } else {
            let acp_servers: Vec<AcpServerConfig> = self
                .acp_servers
                .iter()
                .map(|s| {
                    let mut s = s.clone();
                    s.name = acp_server_display_name(&s, agent_package);
                    s
                })
                .collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_mcp::McpServerConfig;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn mock_mcp_bin() -> PathBuf {
        let exe_name = format!("harnx-mock-mcp{}", std::env::consts::EXE_SUFFIX);
        let current_exe = std::env::current_exe().expect("current test binary path");
        let target_dir = current_exe
            .parent()
            .expect("deps dir")
            .parent()
            .expect("target profile dir");
        let candidate = target_dir.join(&exe_name);
        assert!(
            candidate.exists(),
            "expected mock MCP binary at {}",
            candidate.display()
        );
        candidate
    }

    fn spawn_log_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .map(|contents| {
                contents
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn wait_for_spawn_count(path: &Path, min_lines: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let lines = spawn_log_lines(path);
            if lines.len() >= min_lines {
                return lines;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {} spawn-log lines in {}. current contents: {:?}",
                min_lines,
                path.display(),
                lines
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn force_mcp_spawn(config: &Config) {
        let manager = config
            .mcp_manager
            .as_ref()
            .expect("mcp_manager initialized");
        let tools = manager.get_all_tools_blocking();
        assert!(
            !tools.is_empty(),
            "expected mock MCP server to expose at least one tool"
        );
    }

    /// Regression reproduction for issue #988 residual churn:
    /// `reinit_managers_for_agent` unconditionally rebuilds the MCP manager,
    /// so an agent switch restarts every MCP subprocess even when its spawn
    /// config is unchanged. When fix lands (diff-and-preserve unchanged
    /// servers), flip assertion to `assert_eq!(n1, n2)`.
    #[test]
    fn reinit_managers_restarts_mcp_subprocess_on_agent_switch() {
        let spawn_log = tempfile::NamedTempFile::new().expect("spawn log temp file");
        let spawn_log_path = spawn_log.path().to_path_buf();
        let mock_bin = mock_mcp_bin();

        let mut config = Config {
            mcp_servers: vec![McpServerConfig {
                name: "mock".to_string(),
                command: mock_bin.to_string_lossy().into_owned(),
                args: vec![
                    "--spawn-log".to_string(),
                    spawn_log_path.to_string_lossy().into_owned(),
                ],
                env: Default::default(),
                roots: vec![],
                enabled: true,
                description: None,
                rename_tools: Default::default(),
                tool_templates: Default::default(),
                hooks: None,
                package: None,
            }],
            ..Config::default()
        };

        config.reinit_managers_for_agent(None);
        force_mcp_spawn(&config);
        let first_lines = wait_for_spawn_count(&spawn_log_path, 1);
        let n1 = first_lines.len();

        config.reinit_managers_for_agent(None);
        force_mcp_spawn(&config);
        let second_lines = wait_for_spawn_count(&spawn_log_path, n1 + 1);
        let n2 = second_lines.len();

        assert!(
            n2 > n1,
            "expected reinit_managers_for_agent to restart MCP subprocess; n1={n1}, n2={n2}, log={:?}",
            second_lines
        );
    }
}
