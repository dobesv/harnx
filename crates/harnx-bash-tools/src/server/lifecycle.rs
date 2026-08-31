// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
#![allow(deprecated)]
use super::*;
use anyhow::Context as _;

fn register_tool_templates(
    tool_templates: Vec<ToolTemplate>,
) -> anyhow::Result<BTreeMap<String, RegisteredToolTemplate>> {
    let mut registered = BTreeMap::new();
    for template in tool_templates {
        let (name, entry) = build_registered_tool_template(template)?;
        if registered.insert(name.clone(), entry).is_some() {
            anyhow::bail!("duplicate registered tool template name `{name}`");
        }
    }
    Ok(registered)
}

fn build_registered_tool_template(
    template: ToolTemplate,
) -> anyhow::Result<(String, RegisteredToolTemplate)> {
    if BUILTIN_TOOL_NAMES.contains(&template.name.as_str()) {
        anyhow::bail!(
            "tool template `{}` conflicts with reserved built-in tool `{}`",
            template.name,
            template.name
        );
    }

    let input_schema = template
        .input_schema()?
        .as_object()
        .cloned()
        .context("tool template input schema must be a JSON object")?;
    let sandbox = template.sandbox.clone().unwrap_or_default();
    let has_grants = sandbox_has_grants(&sandbox);
    let (read_paths, write_paths) = expand_template_grant_paths(&template.name, &sandbox)?;
    let name = template.name.clone();
    let description = template
        .description
        .clone()
        .unwrap_or_else(|| format!("Run shell command template `{name}`."));
    let entry = RegisteredToolTemplate {
        template,
        description,
        input_schema,
        read_paths,
        write_paths,
        pass_env: sandbox.env,
        sandbox_enabled: sandbox.enabled,
        no_network: !sandbox.network,
        ignored_grants: !sandbox.enabled && has_grants,
    };
    Ok((name, entry))
}

fn sandbox_has_grants(sandbox: &crate::tool_template::SandboxConfig) -> bool {
    !sandbox.read.is_empty()
        || !sandbox.write.is_empty()
        || !sandbox.env.is_empty()
        || !sandbox.network
}

fn expand_template_grant_paths(
    template_name: &str,
    sandbox: &crate::tool_template::SandboxConfig,
) -> anyhow::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    if !sandbox.enabled {
        return Ok((Vec::new(), Vec::new()));
    }

    let expand_grant = |kind: &str, path: &str| {
        command::expand_path(path).with_context(|| {
            format!("failed to expand {kind} grant `{path}` for tool `{template_name}`")
        })
    };
    let read_paths = sandbox
        .read
        .iter()
        .map(|path| expand_grant("read", path))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let write_paths = sandbox
        .write
        .iter()
        .map(|path| expand_grant("write", path))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((read_paths, write_paths))
}

impl BashServer {
    pub(crate) const DEFAULT_ENV_ALLOWLIST: &[&str] = &[
        "HOME",
        "PATH",
        "LANG",
        "LANGUAGE",
        "USER",
        "SHELL",
        "TERM",
        "DISPLAY",
        "EDITOR",
        "NODE_OPTIONS",
        "NODE_EXTRA_CA_CERTS",
        "PWD",
        "SHLVL",
        "LOGNAME",
        "TMPDIR",
        "TMP",
        "TEMP",
        // Forward Go cache locations so sandboxed `go` honors custom cache dirs
        // that are whitelisted from ambient env in sandbox defaults.
        "GOMODCACHE",
        "GOCACHE",
        // Logging config, so raising the level once raises it for harnx
        // binaries further down the tree — `harnx-sandbox-run` in particular,
        // which this env is built for. HARNX_LOG_PATH is deliberately absent:
        // a sandboxed child usually can't write there, and every binary that
        // reaches this point logs to stderr anyway.
        "HARNX_LOG_LEVEL",
        "HARNX_LOG_FORMAT",
        "HARNX_LOG_FILTER",
        // Windows-specific names. std::env::var returns Err on Unix where
        // these are unset, so listing them here is a no-op on POSIX builds.
        "SYSTEMROOT",
        "SystemRoot",
        "WINDIR",
        "USERPROFILE",
        "USERNAME",
        "APPDATA",
        "LOCALAPPDATA",
        "COMSPEC",
        "HOMEDRIVE",
        "HOMEPATH",
    ];

    #[allow(dead_code)]
    pub fn new(allowlist: ResolvedAllowlist) -> Self {
        let allowlist = Arc::new(allowlist);
        Self::new_with_sandbox(SandboxConfig {
            enabled: false,
            allowlist,
            extra_env_passthrough: vec![],
            env_overrides: vec![],
            sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
        })
    }

    pub fn new_with_sandbox(sandbox_config: SandboxConfig) -> Self {
        Self::new_with_templates(sandbox_config, Vec::new())
            .expect("empty template registration cannot fail")
    }

    pub fn new_with_templates(
        sandbox_config: SandboxConfig,
        tool_templates: Vec<ToolTemplate>,
    ) -> anyhow::Result<Self> {
        let allowlist = sandbox_config.allowlist.clone();
        let log_dir = std::env::temp_dir().join(format!("harnx-bash-tools-{}", Uuid::new_v4()));
        let templates = register_tool_templates(tool_templates)?;
        Ok(Self {
            inner: Arc::new(BashServerInner {
                allowlist,
                spawned: Mutex::new(HashMap::new()),
                log_dir,
                // Writable allowlist entries include broad shared paths such as /tmp.
                // History discovers repositories lazily from actual snapshot paths, so
                // scanning every writable path here is both unnecessary and unbounded.
                history: Arc::new(HistoryManager::new()),
                sandbox_config,
                templates,
            }),
        })
    }

    pub(crate) fn tool_templates(
        &self,
    ) -> impl Iterator<Item = (&String, &RegisteredToolTemplate)> {
        self.inner.templates.iter()
    }

    pub(crate) fn has_tool_template(&self, name: &str) -> bool {
        self.inner.templates.contains_key(name)
    }

    /// Native and MCP stdio modes share the immutable allowlist resolved at startup.
    pub(crate) async fn initialize_allowlist(&self) {}

    pub(crate) async fn ensure_log_dir(&self) -> Result<(), ErrorData> {
        if let Some(parent) = self.inner.log_dir.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|err| {
                internal_error(format!(
                    "failed to create temp parent directory '{}': {err}",
                    parent.display()
                ))
            })?;
        }

        tokio::fs::create_dir_all(&self.inner.log_dir)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to create log directory '{}': {err}",
                    self.inner.log_dir.display()
                ))
            })
    }

    pub(crate) fn next_exec_dir(&self) -> Result<tempfile::TempDir, ErrorData> {
        tempfile::Builder::new()
            .prefix("exec-")
            .tempdir_in(&self.inner.log_dir)
            .map_err(|err| internal_error(format!("failed to create exec directory: {err}")))
    }

    /// Create the per-execution temp dir and open its stdout/stderr log files.
    pub(crate) async fn setup_exec_log(&self) -> Result<ExecLog, ErrorData> {
        self.ensure_log_dir().await?;

        let exec_dir = self.next_exec_dir()?.keep();
        let stdout_log_path = exec_dir.join("stdout.log");
        let stderr_log_path = exec_dir.join("stderr.log");
        let execution_id = exec_dir.file_name().unwrap().to_string_lossy().into_owned();

        let stdout_file = tokio::fs::File::create(&stdout_log_path)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to create stdout log file '{}': {err}",
                    stdout_log_path.display()
                ))
            })?;
        let stderr_file = tokio::fs::File::create(&stderr_log_path)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to create stderr log file '{}': {err}",
                    stderr_log_path.display()
                ))
            })?;

        Ok(ExecLog {
            exec_dir,
            stdout_log_path,
            stderr_log_path,
            execution_id,
            stdout_file,
            stderr_file,
        })
    }

    // -----------------------------------------------------------------------
    // shared helpers
    // -----------------------------------------------------------------------

    pub(crate) async fn resolve_working_dir(
        &self,
        requested: Option<&str>,
    ) -> Result<PathBuf, ErrorData> {
        let current_dir = std::env::current_dir().ok();
        // Empty allowlists and allowlists containing only files have no
        // fallback and remain deny-all for process working directories.
        let default_dir = self
            .inner
            .allowlist
            .default_read_directory(current_dir.as_deref())
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    "no allowed paths are configured for a working directory".to_string(),
                    None,
                )
            })?;

        let resolved = match requested {
            Some(path_str) if !path_str.trim().is_empty() => {
                let path = PathBuf::from(path_str);
                if path.is_absolute() {
                    path
                } else {
                    default_dir.join(path)
                }
            }
            _ => default_dir,
        };

        let canonical = resolved.canonicalize().map_err(|err| {
            ErrorData::invalid_params(
                format!(
                    "cannot resolve working directory '{}': {err}",
                    resolved.display()
                ),
                None,
            )
        })?;

        if !canonical.is_dir() {
            return Err(ErrorData::invalid_params(
                format!(
                    "working directory '{}' is not a directory",
                    canonical.display()
                ),
                None,
            ));
        }

        if !self.inner.allowlist.contains_read(&canonical) {
            return Err(ErrorData::invalid_params(
                format!(
                    "working directory '{}' is outside allowed paths",
                    canonical.display()
                ),
                None,
            ));
        }

        Ok(canonical)
    }

    pub(crate) async fn store_spawned_process(&self, execution_id: String, entry: SpawnedProcess) {
        self.inner.spawned.lock().await.insert(execution_id, entry);
    }

    pub async fn cleanup_log_dir(&self) -> std::io::Result<()> {
        match tokio::fs::remove_dir_all(&self.inner.log_dir).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}
