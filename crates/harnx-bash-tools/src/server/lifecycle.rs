// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
#![allow(deprecated)]
use super::*;

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
        let allowlist = sandbox_config.allowlist.clone();
        let log_dir = std::env::temp_dir().join(format!("harnx-bash-tools-{}", Uuid::new_v4()));
        Self {
            inner: Arc::new(BashServerInner {
                allowlist,
                spawned: Mutex::new(HashMap::new()),
                log_dir,
                // Writable allowlist entries include broad shared paths such as /tmp.
                // History discovers repositories lazily from actual snapshot paths, so
                // scanning every writable path here is both unnecessary and unbounded.
                history: Arc::new(HistoryManager::new(&[])),
                sandbox_config,
            }),
        }
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

    pub fn cleanup_log_dir(&self) -> std::io::Result<()> {
        if self.inner.log_dir.exists() {
            std::fs::remove_dir_all(&self.inner.log_dir)
        } else {
            Ok(())
        }
    }
}
