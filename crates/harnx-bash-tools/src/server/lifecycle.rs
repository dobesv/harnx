// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
// rmcp deprecated the MCP Roots feature (SEP-2577); this server still uses it.
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
    pub fn new(initial_roots: Vec<PathBuf>) -> Self {
        Self::new_with_sandbox(
            initial_roots,
            SandboxConfig {
                enabled: false,
                extra_exec: vec![],
                extra_readable: vec![],
                extra_writable: vec![],
                extra_rwx: vec![],
                extra_env_passthrough: vec![],
                env_overrides: vec![],
                sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
            },
        )
    }

    pub fn new_with_sandbox(initial_roots: Vec<PathBuf>, sandbox_config: SandboxConfig) -> Self {
        Self::new_with_sandbox_and_default_root(initial_roots, sandbox_config, false)
    }

    pub fn new_with_sandbox_and_default_root(
        initial_roots: Vec<PathBuf>,
        sandbox_config: SandboxConfig,
        default_root_cwd: bool,
    ) -> Self {
        let log_dir = std::env::temp_dir().join(format!(
            "harnx-bash-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        Self {
            inner: Arc::new(BashServerInner {
                roots: RwLock::new(initial_roots.clone()),
                initial_roots: initial_roots.clone(),
                roots_initialized: AtomicBool::new(false),
                default_root_cwd,
                spawned: Mutex::new(HashMap::new()),
                log_dir,
                history: Arc::new(HistoryManager::new(&initial_roots)),
                sandbox_config,
            }),
        }
    }

    pub(crate) async fn refresh_roots(
        &self,
        peer: &rmcp::service::Peer<RoleServer>,
    ) -> Result<(), ErrorData> {
        if !peer_supports_roots(peer) {
            // The client never advertised the `roots` capability, so it can't
            // answer a `roots/list` request. Keep the CLI-provided roots.
            self.apply_default_root_cwd_if_empty().await;
            self.inner.roots_initialized.store(true, Ordering::SeqCst);
            return Ok(());
        }

        let result = peer.list_roots().await.map_err(|err| {
            ErrorData::internal_error(format!("failed to fetch roots from peer: {err}"), None)
        })?;

        let peer_roots = result
            .roots
            .into_iter()
            .filter_map(|root| file_uri_to_path(root.uri.as_ref()))
            .collect::<Vec<_>>();

        {
            let mut roots = self.inner.roots.write().await;
            *roots = if peer_roots.is_empty() {
                self.inner.initial_roots.clone()
            } else {
                peer_roots
            };
        }
        self.apply_default_root_cwd_if_empty().await;
        self.inner.roots_initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub(crate) async fn apply_default_root_cwd_if_empty(&self) {
        let mut roots = self.inner.roots.write().await;
        if !roots.is_empty() || !self.inner.default_root_cwd {
            return;
        }

        match default_root_from_cwd() {
            Some(root) => roots.push(root),
            None => eprintln!(
                "harnx-bash-tools: --default-root-cwd: refusing to seed root from CWD \
                 (HOME guard or path resolution failed) — filesystem/exec access denied"
            ),
        }
    }

    pub(crate) async fn initialize_native_roots(&self) {
        self.apply_default_root_cwd_if_empty().await;
        self.inner.roots_initialized.store(true, Ordering::SeqCst);
    }

    pub(crate) async fn ensure_roots_initialized(
        &self,
        peer: &rmcp::service::Peer<RoleServer>,
    ) -> Result<(), ErrorData> {
        if self.inner.roots_initialized.load(Ordering::SeqCst) {
            return Ok(());
        }

        match self.refresh_roots(peer).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if self.inner.roots.read().await.is_empty() {
                    Err(err)
                } else {
                    Ok(())
                }
            }
        }
    }

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
        let roots = self.inner.roots.read().await;
        let default_dir = roots
            .first()
            .cloned()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

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

        if roots.is_empty()
            || !roots.iter().any(|root| {
                root.canonicalize()
                    .map(|canonical_root| canonical.starts_with(&canonical_root))
                    .unwrap_or(false)
            })
        {
            return Err(ErrorData::invalid_params(
                format!(
                    "working directory '{}' is outside allowed roots",
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
