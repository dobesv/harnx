use harnx_mcp::safety::{
    file_uri_to_path, format_size, sanitize_output_text, truncate_output, validate_path,
    validate_write_path, TruncateOpts,
};

use fancy_regex::Regex;
use gix::ObjectId;
use harnx_mcp_history::classify::{classify_command, SnapshotDecision};
use harnx_mcp_history::HistoryManager;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    Meta, PaginatedRequestParams, Role, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::OsString;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::fs::File as TokioFile;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ExecCommandParams {
    command: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    head_lines: Option<usize>,
    #[serde(default)]
    tail_lines: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
    #[serde(default)]
    inputs: Option<Vec<String>>,
    #[serde(default)]
    outputs: Option<Vec<String>>,
}

impl JsonSchema for ExecCommandParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ExecCommandParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let command = generator.subschema_for::<String>();
        let working_dir = generator.subschema_for::<Option<String>>();
        let timeout_secs = generator.subschema_for::<Option<u64>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        let inputs = generator.subschema_for::<Option<Vec<String>>>();
        let outputs = generator.subschema_for::<Option<Vec<String>>>();
        object_schema(
            vec![
                ("command", command),
                ("working_dir", working_dir),
                ("timeout_secs", timeout_secs),
                ("head_lines", head_lines),
                ("tail_lines", tail_lines),
                ("max_output_bytes", max_output_bytes),
                ("inputs", inputs),
                ("outputs", outputs),
            ],
            &["command"],
        )
    }
}

#[derive(Debug, Deserialize)]
struct ReadExecLogParams {
    execution_id: String,
    stream: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    tail: Option<usize>,
    #[serde(default)]
    grep: Option<String>,
    #[serde(default)]
    head_lines: Option<usize>,
    #[serde(default)]
    tail_lines: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

impl JsonSchema for ReadExecLogParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ReadExecLogParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let stream = generator.subschema_for::<String>();
        let offset = generator.subschema_for::<Option<usize>>();
        let limit = generator.subschema_for::<Option<usize>>();
        let tail = generator.subschema_for::<Option<usize>>();
        let grep = generator.subschema_for::<Option<String>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        object_schema(
            vec![
                ("execution_id", execution_id),
                ("stream", stream),
                ("offset", offset),
                ("limit", limit),
                ("tail", tail),
                ("grep", grep),
                ("head_lines", head_lines),
                ("tail_lines", tail_lines),
                ("max_output_bytes", max_output_bytes),
            ],
            &["execution_id", "stream"],
        )
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SpawnCommandParams {
    command: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    inputs: Option<Vec<String>>,
    #[serde(default)]
    outputs: Option<Vec<String>>,
}

impl JsonSchema for SpawnCommandParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SpawnCommandParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let command = generator.subschema_for::<String>();
        let working_dir = generator.subschema_for::<Option<String>>();
        let inputs = generator.subschema_for::<Option<Vec<String>>>();
        let outputs = generator.subschema_for::<Option<Vec<String>>>();
        object_schema(
            vec![
                ("command", command),
                ("working_dir", working_dir),
                ("inputs", inputs),
                ("outputs", outputs),
            ],
            &["command"],
        )
    }
}

#[derive(Debug, Deserialize)]
struct WaitParams {
    execution_id: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    head_lines: Option<usize>,
    #[serde(default)]
    tail_lines: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
    #[serde(default)]
    grep: Option<String>,
}

impl JsonSchema for WaitParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WaitParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let timeout_secs = generator.subschema_for::<Option<u64>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        let grep = generator.subschema_for::<Option<String>>();
        object_schema(
            vec![
                ("execution_id", execution_id),
                ("timeout_secs", timeout_secs),
                ("head_lines", head_lines),
                ("tail_lines", tail_lines),
                ("max_output_bytes", max_output_bytes),
                ("grep", grep),
            ],
            &["execution_id"],
        )
    }
}

#[derive(Debug, Deserialize)]
struct TerminateParams {
    execution_id: String,
    #[serde(default)]
    signal: Option<String>,
}

impl JsonSchema for TerminateParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("TerminateParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let signal = generator.subschema_for::<Option<String>>();
        object_schema(
            vec![("execution_id", execution_id), ("signal", signal)],
            &["execution_id"],
        )
    }
}

#[derive(Debug, Deserialize)]
struct RollbackParams {
    commit_id: String,
    repo_path: String,
}

impl JsonSchema for RollbackParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RollbackParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let commit_id = generator.subschema_for::<String>();
        let repo_path = generator.subschema_for::<String>();
        object_schema(
            vec![("commit_id", commit_id), ("repo_path", repo_path)],
            &["commit_id", "repo_path"],
        )
    }
}

// ---------------------------------------------------------------------------
// Spawned process tracking
// ---------------------------------------------------------------------------

struct SpawnedProcess {
    child: Box<dyn ChildWrapper>,
    command: String,
    working_dir: PathBuf,
    stdout_log_path: PathBuf,
    stderr_log_path: PathBuf,
    before_snap_ids: Vec<(PathBuf, gix::ObjectId)>,
    snapshot_decision: SnapshotDecision,
    /// Resolved output paths from params.outputs; drives history snapshot in wait_impl.
    /// None = use snapshot_decision (classifier); Some([]) = ReadOnly; Some(paths) = Targeted.
    output_paths: Option<Vec<PathBuf>>,
}

/// Configuration for the bash MCP server's sandboxing and child env handling.
///
/// The sandboxing fields (`enabled`, `extra_exec`, `extra_readable`,
/// `sandbox_run_path`) are honoured only on Unix; on other platforms they
/// are accepted for API compatibility and ignored.
///
/// The env-control fields (`extra_env_passthrough`, `env_overrides`) are
/// honoured on every platform — even when sandboxing is unavailable, the
/// child bash process receives only the curated environment.
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub enabled: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub extra_exec: Vec<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub extra_readable: Vec<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub extra_writable: Vec<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub extra_rwx: Vec<PathBuf>,
    /// Extra var names to pass through from host (allowlist additions).
    pub extra_env_passthrough: Vec<String>,
    /// Explicit overrides: KEY → VALUE (highest precedence).
    pub env_overrides: Vec<(String, String)>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub sandbox_run_path: PathBuf,
}

struct BashServerInner {
    roots: RwLock<Vec<PathBuf>>,
    roots_initialized: AtomicBool,
    spawned: Mutex<HashMap<String, SpawnedProcess>>,
    log_dir: PathBuf,
    history: Arc<HistoryManager>,
    /// Sandbox + env config. Sandbox-specific fields (`enabled`,
    /// `extra_exec`, `extra_readable`, `sandbox_run_path`) are only used on
    /// Unix; env fields (`extra_env_passthrough`, `env_overrides`) are
    /// honoured on every platform.
    sandbox_config: SandboxConfig,
}

// ---------------------------------------------------------------------------
// BashServer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BashServer {
    inner: Arc<BashServerInner>,
}

impl BashServer {
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    const SYSTEM_EXEC_PATHS: &[&str] = &[
        "/usr/bin",
        "/bin",
        "/usr/local/bin",
        "/usr/sbin",
        "/sbin",
        "/usr/lib",
        "/usr/lib64",
        "/lib",
        "/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/libexec",
        "/proc",
        "/dev",
        "/sys",
        "/etc",
        "/tmp",
        "/run",
        "/var/run",
        "/usr/share",
    ];
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    const SYSTEM_EXEC_PATHS: &[&str] = &[
        "/usr/bin",
        "/bin",
        "/usr/local/bin",
        "/usr/sbin",
        "/sbin",
        "/usr/lib",
        "/usr/local/lib",
        "/Library",
        "/System",
        "/private/tmp",
        "/private/var",
        "/dev",
    ];
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    #[allow(dead_code)]
    const SYSTEM_EXEC_PATHS: &[&str] = &["/usr/bin", "/bin", "/tmp", "/etc"];

    #[cfg(unix)]
    fn system_writable_paths() -> Vec<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            vec![PathBuf::from("/tmp")]
        }
        #[cfg(target_os = "macos")]
        {
            let mut paths = vec![PathBuf::from("/private/tmp")];
            if let Ok(tmpdir) = std::env::var("TMPDIR") {
                let path = PathBuf::from(&tmpdir);
                if path != Path::new("/private/tmp") {
                    paths.push(path);
                }
            }
            paths
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
        {
            vec![PathBuf::from("/tmp")]
        }
    }

    const DEFAULT_ENV_ALLOWLIST: &[&str] = &[
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
                sandbox_run_path: PathBuf::from("harnx-mcp-bash-sandbox-run"),
            },
        )
    }

    pub fn new_with_sandbox(initial_roots: Vec<PathBuf>, sandbox_config: SandboxConfig) -> Self {
        let log_dir = std::env::temp_dir().join(format!(
            "harnx-bash-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        Self {
            inner: Arc::new(BashServerInner {
                roots: RwLock::new(initial_roots.clone()),
                roots_initialized: AtomicBool::new(false),
                spawned: Mutex::new(HashMap::new()),
                log_dir,
                history: Arc::new(HistoryManager::new(&initial_roots)),
                sandbox_config,
            }),
        }
    }

    async fn refresh_roots(&self, peer: &rmcp::service::Peer<RoleServer>) -> Result<(), ErrorData> {
        let result = peer.list_roots().await.map_err(|err| {
            ErrorData::internal_error(format!("failed to fetch roots from peer: {err}"), None)
        })?;

        let roots = result
            .roots
            .into_iter()
            .filter_map(|root| file_uri_to_path(root.uri.as_ref()))
            .collect::<Vec<_>>();

        let mut guard = self.inner.roots.write().await;
        *guard = roots;
        self.inner.roots_initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn ensure_roots_initialized(
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

    async fn ensure_log_dir(&self) -> Result<(), ErrorData> {
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

    fn next_exec_dir(&self) -> Result<tempfile::TempDir, ErrorData> {
        tempfile::Builder::new()
            .prefix("exec-")
            .tempdir_in(&self.inner.log_dir)
            .map_err(|err| internal_error(format!("failed to create exec directory: {err}")))
    }

    /// Build the child process environment from the configured sources.
    ///
    /// Layers, applied in order from lowest to highest precedence (later
    /// entries replace earlier ones with the same key):
    /// 1. Default allowlist values inherited from the host process env.
    /// 2. `XDG_*` variables inherited from the host process env.
    /// 3. `.env.bash` dotfile values.
    /// 4. `extra_env_passthrough` — host values for explicitly named vars.
    /// 5. `env_overrides` — explicit `KEY=VALUE` overrides.
    ///
    /// This applies on every platform; sandbox-specific behaviour
    /// (birdcage exceptions, `sandbox_run` helper) remains Unix-only.
    fn build_child_env(&self) -> Vec<(String, String)> {
        fn upsert(env_vars: &mut Vec<(String, String)>, key: String, value: String) {
            if let Some((_, existing)) = env_vars.iter_mut().find(|(k, _)| k == &key) {
                *existing = value;
            } else {
                env_vars.push((key, value));
            }
        }

        let mut env_vars: Vec<(String, String)> = Vec::new();

        // 1. Default allowlist.
        for name in Self::DEFAULT_ENV_ALLOWLIST {
            if let Ok(value) = std::env::var(name) {
                upsert(&mut env_vars, (*name).to_string(), value);
            }
        }

        // 2. XDG_* vars from host env.
        for (name, value) in std::env::vars() {
            if name.starts_with("XDG_") {
                upsert(&mut env_vars, name, value);
            }
        }

        // 3. .env.bash dotfile.
        for (key, value) in load_bash_env_file() {
            upsert(&mut env_vars, key, value);
        }

        // 4. Explicit passthrough names — host value wins over dotfile.
        for name in &self.inner.sandbox_config.extra_env_passthrough {
            if let Ok(value) = std::env::var(name) {
                upsert(&mut env_vars, name.clone(), value);
            }
        }

        // 5. Explicit overrides — highest precedence.
        for (key, value) in &self.inner.sandbox_config.env_overrides {
            upsert(&mut env_vars, key.clone(), value.clone());
        }

        env_vars
    }

    #[cfg(unix)]
    fn build_sandbox_args(
        &self,
        working_dir: &Path,
        inputs: Option<&[PathBuf]>,
        outputs: Option<&[PathBuf]>,
        roots: &[PathBuf],
    ) -> Vec<OsString> {
        let mut args = Vec::new();
        let mut readable_paths = Vec::new();
        let mut writable_paths = Vec::new();
        let inputs_explicit_empty = matches!(inputs, Some([]));

        for path in Self::SYSTEM_EXEC_PATHS {
            args.push(OsString::from("--exec"));
            args.push(OsString::from(path));
        }

        for path in Self::system_writable_paths() {
            args.push(OsString::from("--write"));
            args.push(path.into_os_string());
        }

        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            args.push(OsString::from("--exec"));
            args.push(home.join(".local/bin").into_os_string());

            let cargo_home = std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".cargo"));
            args.push(OsString::from("--exec"));
            args.push(cargo_home.join("bin").into_os_string());
        } else if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
            args.push(OsString::from("--exec"));
            args.push(PathBuf::from(cargo_home).join("bin").into_os_string());
        }

        for path in &self.inner.sandbox_config.extra_exec {
            args.push(OsString::from("--exec"));
            args.push(path.clone().into_os_string());
        }

        for path in &self.inner.sandbox_config.extra_readable {
            args.push(OsString::from("--read"));
            args.push(path.clone().into_os_string());
            readable_paths.push(path.clone());
        }

        for path in &self.inner.sandbox_config.extra_writable {
            args.push(OsString::from("--write"));
            args.push(path.clone().into_os_string());
            writable_paths.push(path.clone());
        }

        for path in &self.inner.sandbox_config.extra_rwx {
            args.push(OsString::from("--read"));
            args.push(path.clone().into_os_string());
            args.push(OsString::from("--write"));
            args.push(path.clone().into_os_string());
            args.push(OsString::from("--exec"));
            args.push(path.clone().into_os_string());
            readable_paths.push(path.clone());
            writable_paths.push(path.clone());
        }

        match outputs {
            None => {
                for root in roots {
                    args.push(OsString::from("--write"));
                    args.push(root.clone().into_os_string());
                    args.push(OsString::from("--exec"));
                    args.push(root.clone().into_os_string());
                    writable_paths.push(root.clone());
                }
            }
            Some([]) => {
                if !inputs_explicit_empty {
                    for root in roots {
                        args.push(OsString::from("--read"));
                        args.push(root.clone().into_os_string());
                        args.push(OsString::from("--exec"));
                        args.push(root.clone().into_os_string());
                        readable_paths.push(root.clone());
                    }
                }
            }
            Some(paths) => {
                for path in paths {
                    args.push(OsString::from("--write"));
                    args.push(path.clone().into_os_string());
                    writable_paths.push(path.clone());
                }
                for root in roots {
                    args.push(OsString::from("--exec"));
                    args.push(root.clone().into_os_string());
                }
            }
        }

        if let Some(paths) = inputs {
            if !paths.is_empty() {
                for path in paths {
                    args.push(OsString::from("--read"));
                    args.push(path.clone().into_os_string());
                    readable_paths.push(path.clone());
                }
            }
        }

        if !inputs_explicit_empty {
            let covered = writable_paths
                .iter()
                .chain(readable_paths.iter())
                .any(|path| working_dir.starts_with(path));
            if !covered {
                args.push(OsString::from("--read"));
                args.push(working_dir.as_os_str().to_os_string());
            }
        }

        for (key, value) in self.build_child_env() {
            args.push(OsString::from("--env"));
            args.push(OsString::from(format!("{key}={value}")));
        }

        args
    }

    // -----------------------------------------------------------------------
    // exec
    // -----------------------------------------------------------------------

    async fn exec_command_impl(
        &self,
        params: ExecCommandParams,
    ) -> Result<CallToolResult, ErrorData> {
        if params.command.trim().is_empty() {
            return Err(ErrorData::invalid_params("command cannot be empty", None));
        }

        let working_dir = self
            .resolve_working_dir(params.working_dir.as_deref())
            .await?;
        let snapshot_decision = {
            let roots_guard = self.inner.roots.read().await;
            let resolved = parse_output_path_list(&params.outputs, &roots_guard, &working_dir)?;
            drop(roots_guard);
            match resolved {
                None => classify_command(&params.command, &working_dir),
                Some(paths) if paths.is_empty() => SnapshotDecision::ReadOnly,
                Some(paths) => SnapshotDecision::Targeted(paths),
            }
        };

        // HISTORY: before snapshot (non-fatal)
        let before_snaps = match &snapshot_decision {
            SnapshotDecision::ReadOnly => vec![],
            SnapshotDecision::Targeted(paths) => self
                .inner
                .history
                .snapshot_repos_for_dir_targeted(&working_dir, paths, "before exec")
                .await
                .unwrap_or_else(|e| {
                    log::warn!("history before-snapshot failed: {e}");
                    vec![]
                }),
            SnapshotDecision::FullSnapshot => self
                .inner
                .history
                .snapshot_repos_for_dir(&working_dir, "before exec")
                .await
                .unwrap_or_else(|e| {
                    log::warn!("history before-snapshot failed: {e}");
                    vec![]
                }),
        };

        let timeout_secs = params.timeout_secs.unwrap_or(120);
        let default_opts = TruncateOpts::default();
        let truncate_opts = TruncateOpts {
            head_lines: params.head_lines.unwrap_or(default_opts.head_lines),
            tail_lines: params.tail_lines.unwrap_or(default_opts.tail_lines),
            line_head_bytes: default_opts.line_head_bytes,
            line_tail_bytes: default_opts.line_tail_bytes,
            max_output_bytes: params
                .max_output_bytes
                .unwrap_or(default_opts.max_output_bytes),
            ..default_opts
        };

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

        #[cfg(unix)]
        let use_sandbox = self.inner.sandbox_config.enabled;
        #[cfg(not(unix))]
        let use_sandbox = false;

        let mut command = if use_sandbox {
            #[cfg(unix)]
            {
                let roots_guard = self.inner.roots.read().await;
                let inputs = parse_input_path_list(&params.inputs, &roots_guard, &working_dir)?;
                let outputs = parse_output_path_list(&params.outputs, &roots_guard, &working_dir)?;
                let mut sb_args = self.build_sandbox_args(
                    &working_dir,
                    inputs.as_deref(),
                    outputs.as_deref(),
                    &roots_guard,
                );
                sb_args.push(OsString::from("--working-dir"));
                sb_args.push(working_dir.as_os_str().to_owned());
                sb_args.push(OsString::from("--"));
                sb_args.push(OsString::from("bash"));
                sb_args.push(OsString::from("-c"));
                sb_args.push(OsString::from(&params.command));
                drop(roots_guard);
                let sandbox_run_path = self.inner.sandbox_config.sandbox_run_path.clone();
                CommandWrap::with_new(sandbox_run_path, |command| {
                    command
                        .args(&sb_args)
                        .current_dir(&working_dir)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped());
                })
            }
            #[cfg(not(unix))]
            unreachable!()
        } else {
            let child_env = self.build_child_env();
            CommandWrap::with_new("bash", |command| {
                command
                    .args(["-c", &params.command])
                    .current_dir(&working_dir)
                    .stdin(Stdio::null());
                command.env_clear();
                command.envs(child_env.iter().map(|(k, v)| (k, v)));
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
            })
        };
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        let mut child = command
            .spawn()
            .map_err(|err| internal_error(format!("failed to spawn command: {err}")))?;

        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| internal_error("failed to capture stdout"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| internal_error("failed to capture stderr"))?;

        let stdout_task = tokio::spawn(read_pipe_to_file(stdout, stdout_file));
        let stderr_task = tokio::spawn(read_pipe_to_file(stderr, stderr_file));

        let timeout = Duration::from_secs(timeout_secs);
        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => (Some(status), false),
            Ok(Err(err)) => {
                return Err(internal_error(format!("failed waiting for command: {err}")));
            }
            Err(_) => {
                child.start_kill().map_err(|err| {
                    internal_error(format!("failed to kill command after timeout: {err}"))
                })?;
                match child.wait().await {
                    Ok(status) => (Some(status), true),
                    Err(err) => {
                        return Err(internal_error(format!(
                            "failed waiting for killed command: {err}"
                        )));
                    }
                }
            }
        };

        let stdout_bytes = join_pipe(stdout_task, "stdout").await?;
        let stderr_bytes = join_pipe(stderr_task, "stderr").await?;

        // Sync log files to disk to ensure they're visible to other processes immediately
        if let Ok(f) = tokio::fs::File::open(&stdout_log_path).await {
            let _ = f.sync_all().await;
        }
        if let Ok(f) = tokio::fs::File::open(&stderr_log_path).await {
            let _ = f.sync_all().await;
        }

        let stdout_str = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();
        // exec does not expose a grep param; pass None
        let (streams_block, stdout_lines, stderr_lines, stdout_bytes_len, stderr_bytes_len) =
            render_streams_block(
                &stdout_str,
                &stderr_str,
                &truncate_opts,
                None,
                &execution_id,
                &stdout_log_path,
                &stderr_log_path,
            );
        let total_lines = stdout_lines + stderr_lines;
        let total_bytes = stdout_bytes_len + stderr_bytes_len;

        match (status, timed_out) {
            (Some(status), false) => {
                let exit_code = status.code().unwrap_or(-1);
                let mut output = String::new();
                let _ = writeln!(output, "execution_id: {execution_id}");
                let _ = writeln!(output, "exit_code: {exit_code}");
                let _ = writeln!(output, "working_dir: {}", working_dir.display());
                let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
                let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
                let _ = writeln!(output, "total_lines: {total_lines}");
                let _ = writeln!(
                    output,
                    "total_bytes: {total_bytes} ({})",
                    format_size(total_bytes)
                );
                let _ = write!(output, "\n{streams_block}");
                let summary = format!(
                    "exit_code: {exit_code}, {total_lines} lines, {}",
                    format_size(total_bytes)
                );

                // HISTORY: after snapshot + diff (non-fatal)
                let mut diff_parts: Vec<String> = Vec::new();
                if !before_snaps.is_empty() {
                    let after_snaps = match &snapshot_decision {
                        SnapshotDecision::ReadOnly => vec![],
                        SnapshotDecision::Targeted(paths) => self
                            .inner
                            .history
                            .snapshot_repos_for_dir_targeted(&working_dir, paths, "after exec")
                            .await
                            .unwrap_or_else(|e| {
                                log::warn!("history after-snapshot failed: {e}");
                                vec![]
                            }),
                        SnapshotDecision::FullSnapshot => self
                            .inner
                            .history
                            .snapshot_repos_for_dir(&working_dir, "after exec")
                            .await
                            .unwrap_or_else(|e| {
                                log::warn!("history after-snapshot failed: {e}");
                                vec![]
                            }),
                    };

                    for (repo_dir, before_id) in &before_snaps {
                        if let Some((_, after_id)) = after_snaps.iter().find(|(d, _)| d == repo_dir)
                        {
                            // Always emit snapshot ID
                            if before_id != after_id {
                                match self
                                    .inner
                                    .history
                                    .diff_commits(repo_dir, *before_id, *after_id)
                                    .await
                                {
                                    Ok(diff) if !diff.is_empty() => diff_parts.push(diff),
                                    Ok(_) => {}
                                    Err(e) => log::warn!("history diff failed: {e}"),
                                }
                            }
                        }
                    }
                }

                let mut contents = vec![
                    Content::text(output).with_audience(vec![Role::Assistant]),
                    Content::text(summary).with_audience(vec![Role::User]),
                ];
                for diff in diff_parts {
                    contents.push(Content::text(diff));
                }
                Ok(CallToolResult::success(contents))
            }
            (Some(status), true) => {
                let _ = status;
                tool_error(render_timeout_message(TimeoutRenderContext {
                    working_dir: &working_dir,
                    execution_id: &execution_id,
                    timeout_secs,
                    total_lines,
                    total_bytes,
                    stdout: &stdout_str,
                    stderr: &stderr_str,
                    truncate_opts: &truncate_opts,
                    stdout_log_path: &stdout_log_path,
                    stderr_log_path: &stderr_log_path,
                }))
            }
            (None, _) => tool_error("process exited without status".to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // read_exec_log
    // -----------------------------------------------------------------------

    async fn read_exec_log_impl(
        &self,
        params: ReadExecLogParams,
    ) -> Result<CallToolResult, ErrorData> {
        if params.offset.is_some() && params.tail.is_some() {
            return Err(ErrorData::invalid_params(
                "offset and tail are mutually exclusive",
                None,
            ));
        }

        // Validate stream parameter
        if params.stream != "stdout" && params.stream != "stderr" {
            return Err(ErrorData::invalid_params(
                format!(
                    "stream must be 'stdout' or 'stderr', got '{}'",
                    params.stream
                ),
                None,
            ));
        }

        // Construct absolute path: log_dir/execution_id/stream.log
        // We pass the absolute path string so validate_path uses canonicalize() correctly
        // (passing a relative string would resolve against cwd, not log_dir).
        let abs = self
            .inner
            .log_dir
            .join(&params.execution_id)
            .join(format!("{}.log", params.stream));
        let path = validate_path(
            abs.to_string_lossy().as_ref(),
            std::slice::from_ref(&self.inner.log_dir),
        )
        .map_err(|err| {
            if err.starts_with("Cannot resolve path") {
                ErrorData::invalid_params(
                    format!(
                        "cannot resolve execution_id '{}': {}",
                        params.execution_id, err
                    ),
                    None,
                )
            } else {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is outside the bash server temp log directory",
                        params.execution_id
                    ),
                    None,
                )
            }
        })?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|err| internal_error(format!("cannot access '{}': {err}", path.display())))?;

        if !metadata.is_file() {
            return tool_error(format!("'{}' is not a regular log file.", path.display()));
        }

        let raw_content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| internal_error(format!("failed to read '{}': {err}", path.display())))?;
        // Apply same sanitization as exec/wait output rendering so control characters
        // don't leak through the log-read path.
        let content = sanitize_output_text(&raw_content);

        let grep_regex = match params.grep.as_deref() {
            Some(pattern) => Some(Regex::new(pattern).map_err(|err| {
                ErrorData::invalid_params(format!("invalid grep pattern: {err}"), None)
            })?),
            None => None,
        };

        let mut notices = Vec::new();
        let mut regex_error = None;
        let mut numbered_lines = content
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| {
                let line_number = idx + 1;
                match grep_regex.as_ref() {
                    Some(regex) => match regex.is_match(line) {
                        Ok(true) => Some((line_number, line.to_string())),
                        Ok(false) => None,
                        Err(err) => {
                            if regex_error.is_none() {
                                regex_error = Some(err.to_string());
                            }
                            None
                        }
                    },
                    None => Some((line_number, line.to_string())),
                }
            })
            .collect::<Vec<_>>();

        if let Some(err) = regex_error {
            notices.push(format!("grep evaluation error: {err}"));
        }

        let total_matching_lines = numbered_lines.len();
        if total_matching_lines == 0 {
            let mut output = String::from("<no matching lines>");
            if let Some(pattern) = params.grep.as_deref() {
                let _ = write!(output, "\n\n[no lines matched grep pattern '{}']", pattern);
            }
            let summary = format!("Read {} (0 lines)", path.display());
            return Ok(CallToolResult::success(vec![
                Content::text(output).with_audience(vec![Role::Assistant]),
                Content::text(summary).with_audience(vec![Role::User]),
            ]));
        }

        if let Some(tail) = params.tail {
            if tail < total_matching_lines {
                notices.push(format!(
                    "showing last {} of {} matching lines",
                    tail, total_matching_lines
                ));
                numbered_lines = numbered_lines[total_matching_lines - tail..].to_vec();
            }
        } else if let Some(offset) = params.offset {
            if offset == 0 {
                return Err(ErrorData::invalid_params("offset must be >= 1", None));
            }

            let limit = params.limit.unwrap_or(200).max(1);
            if offset > total_matching_lines {
                return tool_error(format!(
                    "offset {} is beyond the {} matching lines in {}",
                    offset,
                    total_matching_lines,
                    path.display()
                ));
            }

            let start = offset - 1;
            let end = (start + limit).min(total_matching_lines);
            if end < total_matching_lines {
                notices.push(format!(
                    "{} more matching lines. Use offset={} to continue",
                    total_matching_lines - end,
                    end + 1
                ));
            }
            numbered_lines = numbered_lines[start..end].to_vec();
        }

        let raw_output = numbered_lines
            .into_iter()
            .map(|(line_number, line)| format!("{line_number}: {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let default_opts = TruncateOpts::default();
        let truncate_opts = TruncateOpts {
            head_lines: params.head_lines.unwrap_or(default_opts.head_lines),
            tail_lines: params.tail_lines.unwrap_or(default_opts.tail_lines),
            line_head_bytes: default_opts.line_head_bytes,
            line_tail_bytes: default_opts.line_tail_bytes,
            max_output_bytes: params
                .max_output_bytes
                .unwrap_or(default_opts.max_output_bytes),
            ..default_opts
        };
        let truncated_output = truncate_output(&raw_output, &truncate_opts);

        if truncated_output != raw_output {
            notices.push(format!(
                "output truncated from {} to {}. Use head_lines, tail_lines, or max_output_bytes to see more",
                format_size(raw_output.len()),
                format_size(truncated_output.len())
            ));
        }

        let mut output = truncated_output;
        if !notices.is_empty() {
            let _ = write!(output, "\n\n[{}]", notices.join(". "));
        }

        let summary = format!(
            "Read {}/{} ({} lines, {})",
            params.execution_id,
            params.stream,
            total_matching_lines,
            format_size(raw_output.len())
        );
        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    // -----------------------------------------------------------------------
    // spawn
    // -----------------------------------------------------------------------

    async fn spawn_impl(&self, params: SpawnCommandParams) -> Result<CallToolResult, ErrorData> {
        if params.command.trim().is_empty() {
            return Err(ErrorData::invalid_params("command cannot be empty", None));
        }

        let working_dir = self
            .resolve_working_dir(params.working_dir.as_deref())
            .await?;
        let roots_guard = self.inner.roots.read().await;
        let output_paths = parse_output_path_list(&params.outputs, &roots_guard, &working_dir)?;
        drop(roots_guard);
        let snapshot_decision = match &output_paths {
            None => classify_command(&params.command, &working_dir),
            Some(paths) if paths.is_empty() => SnapshotDecision::ReadOnly,
            Some(paths) => SnapshotDecision::Targeted(paths.clone()),
        };

        // HISTORY: before snapshot (non-fatal)
        let before_snap_ids = match &snapshot_decision {
            SnapshotDecision::ReadOnly => vec![],
            SnapshotDecision::Targeted(paths) => self
                .inner
                .history
                .snapshot_repos_for_dir_targeted(&working_dir, paths, "before spawn")
                .await
                .unwrap_or_else(|e| {
                    log::warn!("history before-snapshot failed: {e}");
                    vec![]
                }),
            SnapshotDecision::FullSnapshot => self
                .inner
                .history
                .snapshot_repos_for_dir(&working_dir, "before spawn")
                .await
                .unwrap_or_else(|e| {
                    log::warn!("history before-snapshot failed: {e}");
                    vec![]
                }),
        };

        self.ensure_log_dir().await?;

        let exec_dir = self.next_exec_dir()?.keep();
        let stdout_log_path = exec_dir.join("stdout.log");
        let stderr_log_path = exec_dir.join("stderr.log");
        let execution_id = exec_dir.file_name().unwrap().to_string_lossy().into_owned();

        let stdout_file = std::fs::File::create(&stdout_log_path).map_err(|err| {
            internal_error(format!(
                "failed to create stdout log file '{}': {err}",
                stdout_log_path.display()
            ))
        })?;
        let stderr_file = std::fs::File::create(&stderr_log_path).map_err(|err| {
            internal_error(format!(
                "failed to create stderr log file '{}': {err}",
                stderr_log_path.display()
            ))
        })?;

        #[cfg(unix)]
        let use_sandbox = self.inner.sandbox_config.enabled;
        #[cfg(not(unix))]
        let use_sandbox = false;

        let mut command = if use_sandbox {
            #[cfg(unix)]
            {
                let roots_guard = self.inner.roots.read().await;
                let inputs = parse_input_path_list(&params.inputs, &roots_guard, &working_dir)?;
                let outputs = parse_output_path_list(&params.outputs, &roots_guard, &working_dir)?;
                let mut sb_args = self.build_sandbox_args(
                    &working_dir,
                    inputs.as_deref(),
                    outputs.as_deref(),
                    &roots_guard,
                );
                sb_args.push(OsString::from("--working-dir"));
                sb_args.push(working_dir.as_os_str().to_owned());
                sb_args.push(OsString::from("--"));
                sb_args.push(OsString::from("bash"));
                sb_args.push(OsString::from("-c"));
                sb_args.push(OsString::from(&params.command));
                drop(roots_guard);
                let sandbox_run_path = self.inner.sandbox_config.sandbox_run_path.clone();
                CommandWrap::with_new(sandbox_run_path, |command| {
                    command
                        .args(&sb_args)
                        .current_dir(&working_dir)
                        .stdin(Stdio::null())
                        .stdout(Stdio::from(stdout_file))
                        .stderr(Stdio::from(stderr_file));
                })
            }
            #[cfg(not(unix))]
            unreachable!()
        } else {
            let child_env = self.build_child_env();
            CommandWrap::with_new("bash", |command| {
                command
                    .args(["-c", &params.command])
                    .current_dir(&working_dir)
                    .stdin(Stdio::null());
                command.env_clear();
                command.envs(child_env.iter().map(|(k, v)| (k, v)));
                command
                    .stdout(Stdio::from(stdout_file))
                    .stderr(Stdio::from(stderr_file));
            })
        };
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        let child = command
            .spawn()
            .map_err(|err| internal_error(format!("failed to spawn command: {err}")))?;

        let entry = SpawnedProcess {
            child,
            command: params.command.clone(),
            working_dir: working_dir.clone(),
            stdout_log_path: stdout_log_path.clone(),
            stderr_log_path: stderr_log_path.clone(),
            before_snap_ids,
            snapshot_decision: snapshot_decision.clone(),
            output_paths,
        };

        self.inner
            .spawned
            .lock()
            .await
            .insert(execution_id.clone(), entry);

        let mut output = String::new();
        let _ = writeln!(output, "execution_id: {execution_id}");
        let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
        let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
        let _ = writeln!(output, "working_dir: {}", working_dir.display());
        let _ = write!(output, "command: {}", params.command);
        let summary = format!("spawned {execution_id}");

        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    // -----------------------------------------------------------------------
    // wait
    // -----------------------------------------------------------------------

    async fn wait_impl(&self, params: WaitParams) -> Result<CallToolResult, ErrorData> {
        let timeout_secs = params.timeout_secs.unwrap_or(120);
        let default_opts = TruncateOpts::default();
        let truncate_opts = TruncateOpts {
            head_lines: params.head_lines.unwrap_or(default_opts.head_lines),
            tail_lines: params.tail_lines.unwrap_or(default_opts.tail_lines),
            line_head_bytes: default_opts.line_head_bytes,
            line_tail_bytes: default_opts.line_tail_bytes,
            max_output_bytes: params
                .max_output_bytes
                .unwrap_or(default_opts.max_output_bytes),
            ..default_opts
        };

        let (
            mut child,
            command,
            working_dir,
            stdout_log_path,
            stderr_log_path,
            before_snap_ids,
            snapshot_decision,
            output_paths,
        ) = {
            let mut map = self.inner.spawned.lock().await;
            let entry = map.remove(&params.execution_id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is not a tracked background process (or already waited on)",
                        params.execution_id
                    ),
                    None,
                )
            })?;
            (
                entry.child,
                entry.command,
                entry.working_dir,
                entry.stdout_log_path,
                entry.stderr_log_path,
                entry.before_snap_ids,
                entry.snapshot_decision,
                entry.output_paths,
            )
        };

        let snapshot_decision = match &output_paths {
            None => snapshot_decision,
            Some(paths) if paths.is_empty() => SnapshotDecision::ReadOnly,
            Some(paths) => SnapshotDecision::Targeted(paths.clone()),
        };

        let timeout = Duration::from_secs(timeout_secs);
        let wait_result = tokio::time::timeout(timeout, child.wait()).await;

        // Read both log files
        let stdout_content = tokio::fs::read_to_string(&stdout_log_path)
            .await
            .unwrap_or_default();
        let stderr_content = tokio::fs::read_to_string(&stderr_log_path)
            .await
            .unwrap_or_default();

        let grep_regex = match params.grep.as_deref() {
            Some(pattern) => Some(Regex::new(pattern).map_err(|err| {
                ErrorData::invalid_params(format!("invalid grep pattern: {err}"), None)
            })?),
            None => None,
        };
        let (streams_block, stdout_lines, stderr_lines, stdout_bytes_len, stderr_bytes_len) =
            render_streams_block(
                &stdout_content,
                &stderr_content,
                &truncate_opts,
                grep_regex.as_ref(),
                &params.execution_id,
                &stdout_log_path,
                &stderr_log_path,
            );
        let total_lines = stdout_lines + stderr_lines;
        let total_bytes = stdout_bytes_len + stderr_bytes_len;

        match wait_result {
            Ok(Ok(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                let mut output = String::new();
                let _ = writeln!(output, "execution_id: {}", params.execution_id);
                let _ = writeln!(output, "status: exited");
                let _ = writeln!(output, "exit_code: {exit_code}");
                let _ = writeln!(output, "working_dir: {}", working_dir.display());
                let _ = writeln!(output, "command: {command}");
                let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
                let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
                let _ = writeln!(output, "total_lines: {total_lines}");
                let _ = writeln!(
                    output,
                    "total_bytes: {total_bytes} ({})",
                    format_size(total_bytes)
                );
                let _ = write!(output, "\n{streams_block}");
                let summary = format!(
                    "execution_id '{}' exited with code {exit_code}",
                    params.execution_id
                );

                // HISTORY: after snapshot + diff (non-fatal)
                let mut diff_parts: Vec<String> = Vec::new();
                if !before_snap_ids.is_empty() {
                    let after_snaps = match &snapshot_decision {
                        SnapshotDecision::ReadOnly => vec![],
                        SnapshotDecision::Targeted(paths) => self
                            .inner
                            .history
                            .snapshot_repos_for_dir_targeted(&working_dir, paths, "after wait")
                            .await
                            .unwrap_or_else(|e| {
                                log::warn!("history after-snapshot failed: {e}");
                                vec![]
                            }),
                        SnapshotDecision::FullSnapshot => self
                            .inner
                            .history
                            .snapshot_repos_for_dir(&working_dir, "after wait")
                            .await
                            .unwrap_or_else(|e| {
                                log::warn!("history after-snapshot failed: {e}");
                                vec![]
                            }),
                    };
                    for (repo_dir, before_id) in &before_snap_ids {
                        if let Some((_, after_id)) = after_snaps.iter().find(|(d, _)| d == repo_dir)
                        {
                            // Always emit snapshot ID
                            if before_id != after_id {
                                match self
                                    .inner
                                    .history
                                    .diff_commits(repo_dir, *before_id, *after_id)
                                    .await
                                {
                                    Ok(diff) if !diff.is_empty() => diff_parts.push(diff),
                                    Ok(_) => {}
                                    Err(e) => log::warn!("history diff failed: {e}"),
                                }
                            }
                        }
                    }
                }

                let mut contents = vec![
                    Content::text(output).with_audience(vec![Role::Assistant]),
                    Content::text(summary).with_audience(vec![Role::User]),
                ];
                for diff in diff_parts {
                    contents.push(Content::text(diff));
                }
                Ok(CallToolResult::success(contents))
            }
            Ok(Err(err)) => Err(internal_error(format!(
                "failed waiting for execution_id '{}': {err}",
                params.execution_id
            ))),
            Err(_) => {
                let mut map = self.inner.spawned.lock().await;
                map.insert(
                    params.execution_id.clone(),
                    SpawnedProcess {
                        child,
                        command: command.clone(),
                        working_dir: working_dir.clone(),
                        stdout_log_path: stdout_log_path.clone(),
                        stderr_log_path: stderr_log_path.clone(),
                        before_snap_ids,
                        snapshot_decision,
                        output_paths,
                    },
                );

                let mut output = String::new();
                let _ = writeln!(output, "execution_id: {}", params.execution_id);
                let _ = writeln!(output, "status: running");
                let _ = writeln!(output, "working_dir: {}", working_dir.display());
                let _ = writeln!(output, "command: {command}");
                let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
                let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
                let _ = writeln!(output, "total_lines: {total_lines}");
                let _ = writeln!(
                    output,
                    "total_bytes: {total_bytes} ({})",
                    format_size(total_bytes)
                );
                let _ = write!(output, "\n{streams_block}");
                let summary = format!(
                    "execution_id '{}' still running after {}s",
                    params.execution_id, timeout_secs
                );
                Ok(CallToolResult::success(vec![
                    Content::text(output).with_audience(vec![Role::Assistant]),
                    Content::text(summary).with_audience(vec![Role::User]),
                ]))
            }
        }
    }

    // -----------------------------------------------------------------------
    // terminate
    // -----------------------------------------------------------------------

    async fn terminate_impl(&self, params: TerminateParams) -> Result<CallToolResult, ErrorData> {
        let normalized = params.signal.as_deref().unwrap_or("SIGTERM").to_uppercase();

        #[cfg(unix)]
        {
            let (signum, signal_name) = match normalized.as_str() {
                "SIGTERM" | "TERM" => (libc::SIGTERM, "SIGTERM"),
                "SIGKILL" | "KILL" => (libc::SIGKILL, "SIGKILL"),
                "SIGINT" | "INT" => (libc::SIGINT, "SIGINT"),
                "SIGHUP" | "HUP" => (libc::SIGHUP, "SIGHUP"),
                other => {
                    return Err(ErrorData::invalid_params(
                        format!("unsupported signal: {other}"),
                        None,
                    ));
                }
            };

            let map = self.inner.spawned.lock().await;
            let entry = map.get(&params.execution_id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is not a tracked background process",
                        params.execution_id
                    ),
                    None,
                )
            })?;

            entry.child.signal(signum).map_err(|err| {
                internal_error(format!(
                    "failed to send {signal_name} to execution_id '{}': {err}",
                    params.execution_id
                ))
            })?;

            let mut output = String::new();
            let _ = writeln!(output, "execution_id: {}", params.execution_id);
            let _ = writeln!(output, "signal: {}", normalized);
            let _ = writeln!(output, "command: {}", entry.command);
            let _ = writeln!(
                output,
                "stdout_log_path: {}",
                entry.stdout_log_path.display()
            );
            let _ = write!(
                output,
                "stderr_log_path: {}",
                entry.stderr_log_path.display()
            );
            let summary = format!("sent {} to {}", normalized, params.execution_id);

            Ok(CallToolResult::success(vec![
                Content::text(output).with_audience(vec![Role::Assistant]),
                Content::text(summary).with_audience(vec![Role::User]),
            ]))
        }

        #[cfg(windows)]
        {
            let mut map = self.inner.spawned.lock().await;
            let entry = map.get_mut(&params.execution_id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is not a tracked background process",
                        params.execution_id
                    ),
                    None,
                )
            })?;

            if let Err(e) = entry.child.start_kill() {
                return Err(internal_error(format!(
                    "failed to terminate execution_id '{}': {e}",
                    params.execution_id
                )));
            }

            let command = entry.command.clone();
            let stdout_log_path = entry.stdout_log_path.clone();
            let stderr_log_path = entry.stderr_log_path.clone();

            let mut output = String::new();
            let _ = writeln!(output, "execution_id: {}", params.execution_id);
            let _ = writeln!(output, "signal: {}", normalized);
            let _ = writeln!(output, "command: {}", command);
            let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
            let _ = write!(output, "stderr_log_path: {}", stderr_log_path.display());
            let summary = format!("sent {} to {}", normalized, params.execution_id);

            Ok(CallToolResult::success(vec![
                Content::text(output).with_audience(vec![Role::Assistant]),
                Content::text(summary).with_audience(vec![Role::User]),
            ]))
        }
    }

    // -----------------------------------------------------------------------
    // rollback_file
    // -----------------------------------------------------------------------

    async fn rollback_file_impl(
        &self,
        params: RollbackParams,
    ) -> Result<CallToolResult, ErrorData> {
        let roots = self.inner.roots.read().await;
        let path = validate_path(&params.repo_path, &roots).map_err(invalid_params)?;
        drop(roots);

        let commit_id = ObjectId::from_hex(params.commit_id.as_bytes())
            .map_err(|e| ErrorData::invalid_params(format!("invalid commit_id: {e}"), None))?;

        let repo_dir = harnx_mcp_history::discover::find_repo_for_path(&path).ok_or_else(|| {
            ErrorData::invalid_params("path is not inside a git repository".to_string(), None)
        })?;

        let new_commit_id = self
            .inner
            .history
            .rollback(&repo_dir, commit_id)
            .await
            .map_err(|e| ErrorData::internal_error(format!("rollback failed: {e}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Rolled back to harnx snapshot {}; new commit {} created (can be reverted)",
            &params.commit_id[..8.min(params.commit_id.len())],
            new_commit_id.to_hex(),
        ))]))
    }

    // -----------------------------------------------------------------------
    // shared helpers
    // -----------------------------------------------------------------------

    async fn resolve_working_dir(&self, requested: Option<&str>) -> Result<PathBuf, ErrorData> {
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

    pub fn cleanup_log_dir(&self) -> std::io::Result<()> {
        if self.inner.log_dir.exists() {
            std::fs::remove_dir_all(&self.inner.log_dir)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler impl
// ---------------------------------------------------------------------------

impl ServerHandler for BashServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-mcp-bash",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Local shell command MCP server with output truncation and retrievable temp logs.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            meta: None,
            tools: vec![
                Tool::new(
                    "exec",
                    "Execute a local bash command and return truncated combined stdout/stderr. When output is cropped, stdout/stderr temp log files are included for later retrieval.",
                    Map::new(),
                )
                .with_input_schema::<ExecCommandParams>()
                .with_meta(Meta(json!({
                    "call_template": "**$** `{{ args.command }}`{% if args.working_dir %} *(in {{ args.working_dir }})*{% endif %}",
                }).as_object().unwrap().clone())),
                Tool::new(
                    "read_exec_log",
                    "Read a temp stdout/stderr log file generated by this bash server. Use execution_id from exec/wait response with stream 'stdout' or 'stderr'. Supports offset/limit/tail/grep/head_lines/tail_lines/max_output_bytes, but only for server-owned temp logs.",
                    Map::new(),
                )
                .with_input_schema::<ReadExecLogParams>()
                .with_meta(Meta(json!({
                    "call_template": "**read log** `{{ args.execution_id }}/{{ args.stream }}`",
                }).as_object().unwrap().clone())),
                Tool::new(
                    "spawn",
                    "Spawn a background bash command. Returns an execution_id and log file paths immediately without waiting for the command to finish. Output is written to separate stdout.log and stderr.log files. Use 'wait' to check for completion and 'terminate' to stop it.",
                    Map::new(),
                )
                .with_input_schema::<SpawnCommandParams>()
                .with_meta(Meta(json!({
                    "call_template": "**spawn** `{{ args.command }}`{% if args.working_dir %} *(in {{ args.working_dir }})*{% endif %}",
                }).as_object().unwrap().clone())),
                Tool::new(
                    "wait",
                    "Wait for a spawned background process to exit. Returns the exit code, output metrics, and truncated output. If the process does not exit within the timeout, returns its current status and partial output without killing it.",
                    Map::new(),
                )
                .with_input_schema::<WaitParams>()
                .with_meta(Meta(json!({
                    "call_template": "**wait** for {{ args.execution_id }}",
                }).as_object().unwrap().clone())),
                Tool::new(
                    "terminate",
                    "Send a signal to a spawned background process. Default signal is SIGTERM. Supported signals: SIGTERM, SIGKILL, SIGINT, SIGHUP.",
                    Map::new(),
                )
                .with_input_schema::<TerminateParams>()
                .with_meta(Meta(json!({
                    "call_template": "**terminate** {{ args.execution_id }}{% if args.signal %} ({{ args.signal }}){% endif %}",
                }).as_object().unwrap().clone())),
                Tool::new(
                    "rollback_file",
                    "Restore a repository to a prior harnx history snapshot. Pass the commit SHA from the 'commit <sha>' line at the top of a prior tool response's diff as the commit_id parameter.",
                    Map::new(),
                )
                .with_input_schema::<RollbackParams>()
                .with_meta(Meta(json!({
                    "call_template": "**rollback_file** to `{{ args.commit_id | truncate(8, end='') }}`",
                }).as_object().unwrap().clone())),
            ],
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(err) = self.ensure_roots_initialized(&context.peer).await {
            eprintln!(
                "harnx-mcp-bash: failed to initialize roots: {}",
                err.message
            );
        }

        match request.name.as_ref() {
            "exec" => {
                let params = parse_arguments::<ExecCommandParams>(request.arguments)?;
                self.exec_command_impl(params).await
            }
            "read_exec_log" => {
                let params = parse_arguments::<ReadExecLogParams>(request.arguments)?;
                self.read_exec_log_impl(params).await
            }
            "spawn" => {
                let params = parse_arguments::<SpawnCommandParams>(request.arguments)?;
                self.spawn_impl(params).await
            }
            "wait" => {
                let params = parse_arguments::<WaitParams>(request.arguments)?;
                self.wait_impl(params).await
            }
            "terminate" => {
                let params = parse_arguments::<TerminateParams>(request.arguments)?;
                self.terminate_impl(params).await
            }
            "rollback_file" => {
                let params = parse_arguments::<RollbackParams>(request.arguments)?;
                self.rollback_file_impl(params).await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    fn on_roots_list_changed(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let this = self.clone();
        async move {
            let peer = context.peer.clone();
            tokio::spawn(async move {
                if let Err(err) = this.refresh_roots(&peer).await {
                    eprintln!("harnx-mcp-bash: failed to refresh roots: {}", err.message);
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn parse_arguments<T: DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(format!("invalid tool arguments: {err}"), None))
}

fn tool_error(msg: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![Content::text(msg.into())]))
}

fn invalid_params(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(msg, None)
}

fn internal_error(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::internal_error(msg, None)
}

#[cfg(all(test, target_os = "linux"))]
fn sandbox_run_test_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/harnx-mcp-bash-sandbox-run")
}

#[cfg(unix)]
fn parse_input_path_list(
    list: &Option<Vec<String>>,
    roots: &[PathBuf],
    working_dir: &Path,
) -> Result<Option<Vec<PathBuf>>, ErrorData> {
    parse_validated_path_list(list, roots, working_dir, validate_path)
}

fn parse_output_path_list(
    list: &Option<Vec<String>>,
    roots: &[PathBuf],
    working_dir: &Path,
) -> Result<Option<Vec<PathBuf>>, ErrorData> {
    parse_validated_path_list(list, roots, working_dir, validate_write_path)
}

fn parse_validated_path_list(
    list: &Option<Vec<String>>,
    roots: &[PathBuf],
    working_dir: &Path,
    validator: fn(&str, &[PathBuf]) -> Result<PathBuf, String>,
) -> Result<Option<Vec<PathBuf>>, ErrorData> {
    match list {
        None => Ok(None),
        Some(strs) => {
            let mut out = Vec::with_capacity(strs.len());
            for raw in strs {
                if raw.trim().is_empty() {
                    return Err(ErrorData::invalid_params(
                        "path list contains empty string",
                        None,
                    ));
                }
                let resolved = if Path::new(raw).is_relative() {
                    working_dir.join(raw)
                } else {
                    PathBuf::from(raw)
                };
                let validated = validator(&resolved.to_string_lossy(), roots)
                    .map_err(|err| ErrorData::invalid_params(err, None))?;
                out.push(validated);
            }
            Ok(Some(out))
        }
    }
}

fn load_bash_env_file() -> Vec<(String, String)> {
    fn bash_config_dir() -> PathBuf {
        if let Ok(v) = std::env::var("HARNX_CONFIG_DIR") {
            return PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("XDG_CONFIG_HOME") {
            return PathBuf::from(v).join("harnx");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".config/harnx");
        }
        PathBuf::from(".config/harnx")
    }

    let env_file = bash_config_dir().join(".env.bash");
    let Ok(contents) = std::fs::read_to_string(env_file) else {
        return vec![];
    };

    contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }

            let mut parts = trimmed.splitn(2, '=');
            let key = parts.next()?.trim();
            let value = parts.next()?.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn object_schema(properties: Vec<(&str, Schema)>, required: &[&str]) -> Schema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));

    let mut property_map = Map::new();
    for (name, property_schema) in properties {
        property_map.insert(name.to_string(), property_schema.as_value().clone());
    }
    schema.insert("properties".to_string(), Value::Object(property_map));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));

    if !required.is_empty() {
        schema.insert(
            "required".to_string(),
            Value::Array(
                required
                    .iter()
                    .map(|name| Value::String((*name).to_string()))
                    .collect(),
            ),
        );
    }

    schema.into()
}

async fn read_pipe_to_file<R>(mut reader: R, mut writer: TokioFile) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        writer.write_all(&buffer[..read]).await?;
        bytes.extend_from_slice(&buffer[..read]);
    }

    writer.flush().await?;
    writer.sync_all().await?;
    Ok(bytes)
}

async fn join_pipe(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    name: &str,
) -> Result<Vec<u8>, ErrorData> {
    task.await
        .map_err(|err| internal_error(format!("failed to join {name} reader task: {err}")))?
        .map_err(|err| internal_error(format!("failed to read {name}: {err}")))
}

fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count()
    }
}

/// Render one stream's output block with `===== stdout =====` / `===== /stdout =====` markers.
/// Each stream is truncated independently using `truncate_opts`.
/// Returns the rendered block string (no trailing newline).
fn render_stream_block(
    name: &str,
    content: &str,
    truncate_opts: &TruncateOpts,
    log_hint: Option<(&str, &Path)>, // (execution_id, log_path) for truncation hint
) -> String {
    let sanitized = sanitize_output_text(content);
    if sanitized.is_empty() {
        return format!("===== {name} (empty) =====");
    }
    let truncated = truncate_output(&sanitized, truncate_opts);
    let was_truncated = truncated != sanitized;
    let mut block = format!("===== {name} =====\n{truncated}");
    if was_truncated {
        if let Some((execution_id, log_path)) = log_hint {
            let _ = write!(
                block,
                "\n\n[{name} truncated from {} to {}. Use max_output_bytes, head_lines, or tail_lines to see more; full log via read_exec_log: execution_id={execution_id}, stream={name} ({})]",
                format_size(sanitized.len()),
                format_size(truncated.len()),
                log_path.display()
            );
        } else {
            let _ = write!(
                block,
                "\n\n[{name} truncated from {} to {}. Use max_output_bytes, head_lines, or tail_lines to see more]",
                format_size(sanitized.len()),
                format_size(truncated.len())
            );
        }
    }
    let _ = write!(block, "\n===== /{name} =====");
    block
}

/// Render separate stdout and stderr blocks, each truncated independently.
/// Returns (rendered_string, stdout_lines, stderr_lines, stdout_bytes, stderr_bytes).
/// Apply a grep regex filter to each line of `content`, returning only matching lines joined by `\n`.
/// Lines that fail regex evaluation are kept (fail-open).
fn grep_filter(content: &str, grep_regex: &Regex) -> String {
    content
        .lines()
        .filter(|line| grep_regex.is_match(line).unwrap_or(true))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render separate stdout and stderr blocks, each grep-filtered and truncated independently.
/// Returns (rendered_string, stdout_lines, stderr_lines, stdout_bytes, stderr_bytes).
/// Metrics reflect post-grep content so callers see accurate totals.
fn render_streams_block(
    stdout: &str,
    stderr: &str,
    truncate_opts: &TruncateOpts,
    grep_regex: Option<&Regex>,
    execution_id: &str,
    stdout_log_path: &Path,
    stderr_log_path: &Path,
) -> (String, usize, usize, usize, usize) {
    let stdout_filtered = grep_regex
        .map(|r| grep_filter(stdout, r))
        .unwrap_or_else(|| stdout.to_owned());
    let stderr_filtered = grep_regex
        .map(|r| grep_filter(stderr, r))
        .unwrap_or_else(|| stderr.to_owned());

    let stdout_lines = count_lines(&stdout_filtered);
    let stderr_lines = count_lines(&stderr_filtered);
    let stdout_bytes = stdout_filtered.len();
    let stderr_bytes = stderr_filtered.len();

    let stdout_block = render_stream_block(
        "stdout",
        &stdout_filtered,
        truncate_opts,
        Some((execution_id, stdout_log_path)),
    );
    let stderr_block = render_stream_block(
        "stderr",
        &stderr_filtered,
        truncate_opts,
        Some((execution_id, stderr_log_path)),
    );

    let rendered = format!("{stdout_block}\n{stderr_block}");
    (
        rendered,
        stdout_lines,
        stderr_lines,
        stdout_bytes,
        stderr_bytes,
    )
}

struct TimeoutRenderContext<'a> {
    working_dir: &'a Path,
    execution_id: &'a str,
    timeout_secs: u64,
    total_lines: usize,
    total_bytes: usize,
    stdout: &'a str,
    stderr: &'a str,
    truncate_opts: &'a TruncateOpts,
    stdout_log_path: &'a Path,
    stderr_log_path: &'a Path,
}

fn render_timeout_message(ctx: TimeoutRenderContext<'_>) -> String {
    let TimeoutRenderContext {
        working_dir,
        execution_id,
        timeout_secs,
        total_lines,
        total_bytes,
        stdout,
        stderr,
        truncate_opts,
        stdout_log_path,
        stderr_log_path,
    } = ctx;
    let mut output = String::new();
    let _ = writeln!(
        output,
        "command timed out after {timeout_secs}s and was terminated"
    );
    let _ = writeln!(output, "execution_id: {execution_id}");
    let _ = writeln!(output, "working_dir: {}", working_dir.display());
    let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
    let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
    let _ = writeln!(output, "total_lines: {total_lines}");
    let _ = writeln!(
        output,
        "total_bytes: {total_bytes} ({})",
        format_size(total_bytes)
    );
    let (streams_block, _, _, _, _) = render_streams_block(
        stdout,
        stderr,
        truncate_opts,
        None,
        execution_id,
        stdout_log_path,
        stderr_log_path,
    );
    let _ = write!(output, "\n{streams_block}");
    output
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::ffi::{OsStr, OsString};
    #[cfg(unix)]
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{ClientCapabilities, InitializeRequestParams, ListRootsResult, Root};
    use rmcp::service::{
        serve_client, serve_server, RequestContext, RoleClient, RoleServer, RunningService,
    };
    use tokio::io::duplex;
    use uuid::Uuid;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("harnx-mcp-bash-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Clone, Default)]
    #[allow(dead_code)]
    struct TestClientHandler {
        roots: Vec<PathBuf>,
    }

    impl TestClientHandler {
        #[allow(dead_code)]
        fn new(roots: Vec<PathBuf>) -> Self {
            Self { roots }
        }
    }

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> InitializeRequestParams {
            InitializeRequestParams::new(
                ClientCapabilities::builder()
                    .enable_roots()
                    .enable_roots_list_changed()
                    .build(),
                Implementation::new("test", "0.1"),
            )
        }

        async fn list_roots(
            &self,
            _cx: RequestContext<RoleClient>,
        ) -> Result<ListRootsResult, ErrorData> {
            Ok(ListRootsResult::new(
                self.roots
                    .iter()
                    .map(|root| {
                        Root::new(format!("file://{}", root.canonicalize().unwrap().display()))
                    })
                    .collect(),
            ))
        }
    }

    #[allow(dead_code)]
    struct TestConnection {
        _server_service: RunningService<RoleServer, BashServer>,
        client_service: RunningService<RoleClient, TestClientHandler>,
    }

    #[allow(dead_code)]
    async fn connect_server(server: BashServer, roots: Vec<PathBuf>) -> TestConnection {
        let (client_transport, server_transport) = duplex(65_536);
        let server_fut = serve_server(server, server_transport);
        let client_fut = serve_client(TestClientHandler::new(roots), client_transport);
        let (server_res, client_res) = tokio::join!(server_fut, client_fut);

        TestConnection {
            _server_service: server_res.unwrap(),
            client_service: client_res.unwrap(),
        }
    }

    fn text_content(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| match &content.raw {
                rmcp::model::RawContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn bash_tools_advertise_call_template_only() {
        // Each tool ships a `_meta.call_template` for the TUI's call header.
        // We deliberately omit `result_template` so the MCP client falls
        // back to its audience-aware generic renderer — that's what surfaces
        // the history diff content blocks (issue #398).
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let server = BashServer::new_with_sandbox(
            vec![temp_dir.path().to_path_buf()],
            disabled_sandbox_config(),
        );
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(server, vec![temp_dir.path().to_path_buf()]).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let tools = peer.list_tools(Default::default()).await.unwrap().tools;
        assert!(!tools.is_empty(), "server should expose at least one tool");
        for tool in &tools {
            let meta = tool
                .meta
                .as_ref()
                .unwrap_or_else(|| panic!("tool '{}' has no _meta", tool.name));
            assert!(
                meta.0.contains_key("call_template"),
                "tool '{}' missing call_template in _meta",
                tool.name
            );
            assert!(
                !meta.0.contains_key("result_template"),
                "tool '{}' must not pin result_template — let the client fall back to its generic audience-aware renderer",
                tool.name
            );
        }
    }

    #[cfg(unix)]
    fn collect_arg_pairs(args: &[OsString]) -> Vec<(String, String)> {
        args.chunks(2)
            .filter_map(|w| {
                if w.len() == 2 {
                    Some((
                        w[0].to_string_lossy().into_owned(),
                        w[1].to_string_lossy().into_owned(),
                    ))
                } else {
                    None
                }
            })
            .collect()
    }

    #[cfg(unix)]
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        match LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[cfg(unix)]
    struct EnvVar {
        key: String,
        prev: Option<OsString>,
    }

    #[cfg(unix)]
    impl EnvVar {
        fn set(key: &str, value: impl AsRef<OsStr>) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value.as_ref()) };
            Self {
                key: key.to_string(),
                prev,
            }
        }

        fn unset(key: &str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self {
                key: key.to_string(),
                prev,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for EnvVar {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var(&self.key, value),
                    None => std::env::remove_var(&self.key),
                }
            }
        }
    }

    #[cfg(unix)]
    fn enabled_sandbox_config() -> SandboxConfig {
        SandboxConfig {
            enabled: true,
            extra_exec: vec![],
            extra_readable: vec![],
            extra_writable: vec![],
            extra_rwx: vec![],
            extra_env_passthrough: vec![],
            env_overrides: vec![],
            sandbox_run_path: PathBuf::from("harnx-mcp-bash-sandbox-run"),
        }
    }

    #[cfg(unix)]
    fn disabled_sandbox_config() -> SandboxConfig {
        SandboxConfig {
            enabled: false,
            ..enabled_sandbox_config()
        }
    }

    #[cfg(target_os = "linux")]
    fn sandboxed_server(roots: Vec<PathBuf>) -> BashServer {
        BashServer::new_with_sandbox(
            roots,
            SandboxConfig {
                enabled: true,
                extra_exec: vec![],
                extra_readable: vec![],
                extra_writable: vec![],
                extra_rwx: vec![],
                extra_env_passthrough: vec![],
                env_overrides: vec![],
                sandbox_run_path: sandbox_run_test_path(),
            },
        )
    }

    /// Probe whether birdcage's sandbox can actually initialize in the current
    /// environment. GitHub Actions Ubuntu runners and other restricted Linux
    /// environments commonly disallow unprivileged user namespaces, which
    /// causes `Sandbox::spawn()` to fail with EPERM at runtime. The
    /// sandbox-runtime tests below short-circuit and log a "skipping" message
    /// when this returns false, instead of failing the build.
    #[cfg(target_os = "linux")]
    fn sandbox_runtime_works() -> bool {
        let helper = sandbox_run_test_path();
        if !helper.exists() {
            eprintln!(
                "sandbox runtime probe: helper not built at {} — skipping",
                helper.display()
            );
            return false;
        }
        let output = std::process::Command::new(&helper)
            .args([
                "--exec",
                "/usr/bin",
                "--exec",
                "/bin",
                "--exec",
                "/lib",
                "--exec",
                "/lib64",
                "--exec",
                "/usr/lib",
                "--exec",
                "/usr/lib64",
                "--exec",
                "/usr/lib/x86_64-linux-gnu",
                "--exec",
                "/etc",
                "--exec",
                "/proc",
                "--exec",
                "/dev",
                "--exec",
                "/tmp",
                "--exec",
                "/usr/share",
                "--working-dir",
                "/tmp",
                "--",
                "bash",
                "-c",
                "exit 0",
            ])
            .output();
        match output {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                eprintln!(
                    "sandbox runtime probe: birdcage cannot initialize here (exit={:?}, stderr={:?}) — skipping",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                false
            }
            Err(err) => {
                eprintln!("sandbox runtime probe: failed to spawn helper: {err} — skipping");
                false
            }
        }
    }

    fn extract_field(text: &str, field: &str) -> String {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{field}: ")))
            .unwrap()
            .to_string()
    }

    async fn git(args: &[&str], cwd: &Path) {
        let status = tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .await
            .unwrap();
        assert!(
            status.success(),
            "git command failed: git {}",
            args.join(" ")
        );
    }

    async fn init_git_repo(root: &Path) {
        tokio::fs::write(root.join("tracked.txt"), "baseline\n")
            .await
            .unwrap();
        git(&["init"], root).await;
        git(&["config", "user.name", "Test User"], root).await;
        git(&["config", "user.email", "test@example.com"], root).await;
        git(&["add", "tracked.txt"], root).await;
        git(&["commit", "-m", "initial"], root).await;
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_args_defaults() {
        let server = BashServer::new_with_sandbox(
            vec![PathBuf::from("/test/root")],
            enabled_sandbox_config(),
        );
        let args = server.build_sandbox_args(
            Path::new("/test/root/workdir"),
            None,
            None,
            &[PathBuf::from("/test/root")],
        );
        let pairs = collect_arg_pairs(&args);

        assert!(pairs.contains(&("--write".into(), "/test/root".into())));
        assert!(pairs.contains(&("--exec".into(), "/usr/bin".into())));
        assert!(!pairs.contains(&("--write".into(), "/usr/bin".into())));
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_args_empty_outputs() {
        let server = BashServer::new_with_sandbox(
            vec![PathBuf::from("/test/root")],
            enabled_sandbox_config(),
        );
        let empty: [PathBuf; 0] = [];
        let args = server.build_sandbox_args(
            Path::new("/test/root/workdir"),
            None,
            Some(&empty),
            &[PathBuf::from("/test/root")],
        );
        let pairs = collect_arg_pairs(&args);

        assert!(pairs.contains(&("--read".into(), "/test/root".into())));
        assert!(!pairs.contains(&("--write".into(), "/test/root".into())));
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_args_custom_outputs() {
        let server = BashServer::new_with_sandbox(
            vec![PathBuf::from("/test/root")],
            enabled_sandbox_config(),
        );
        let outputs = [PathBuf::from("/custom/out")];
        let args = server.build_sandbox_args(
            Path::new("/custom/out/workdir"),
            None,
            Some(&outputs),
            &[PathBuf::from("/test/root")],
        );
        let pairs = collect_arg_pairs(&args);

        assert!(pairs.contains(&("--write".into(), "/custom/out".into())));
        assert!(!pairs.contains(&("--write".into(), "/test/root".into())));
        assert!(!pairs.contains(&("--read".into(), "/test/root".into())));
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_args_empty_inputs_empty_outputs() {
        let server = BashServer::new_with_sandbox(
            vec![PathBuf::from("/test/root")],
            enabled_sandbox_config(),
        );
        let empty: [PathBuf; 0] = [];
        let working_dir = PathBuf::from("/tmp/test_wd_xxx");
        let args = server.build_sandbox_args(
            &working_dir,
            Some(&empty),
            Some(&empty),
            &[PathBuf::from("/test/root")],
        );
        let pairs = collect_arg_pairs(&args);

        assert!(!pairs.contains(&("--write".into(), "/test/root".into())));
        assert!(!pairs.contains(&("--read".into(), "/test/root".into())));
        assert!(!pairs.contains(&("--read".into(), "/tmp/test_wd_xxx".into())));
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_args_extra_writable() {
        let mut config = enabled_sandbox_config();
        config
            .extra_writable
            .push(PathBuf::from("/custom/writable"));
        let server = BashServer::new_with_sandbox(vec![PathBuf::from("/test/root")], config);
        let args = server.build_sandbox_args(
            Path::new("/custom/writable/workdir"),
            None,
            None,
            &[PathBuf::from("/test/root")],
        );
        let pairs = collect_arg_pairs(&args);

        assert!(pairs.contains(&("--write".into(), "/custom/writable".into())));
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_args_extra_rwx() {
        let mut config = enabled_sandbox_config();
        config.extra_rwx.push(PathBuf::from("/custom/rwx"));
        let server = BashServer::new_with_sandbox(vec![PathBuf::from("/test/root")], config);
        let args = server.build_sandbox_args(
            Path::new("/custom/rwx/workdir"),
            None,
            None,
            &[PathBuf::from("/test/root")],
        );
        let pairs = collect_arg_pairs(&args);

        assert!(pairs.contains(&("--read".into(), "/custom/rwx".into())));
        assert!(pairs.contains(&("--write".into(), "/custom/rwx".into())));
        assert!(pairs.contains(&("--exec".into(), "/custom/rwx".into())));
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_args_roots_get_exec() {
        let root = PathBuf::from("/test/root");
        let server = BashServer::new_with_sandbox(vec![root.clone()], enabled_sandbox_config());
        let args = server.build_sandbox_args(Path::new("/test/root/workdir"), None, None, &[root]);
        let pairs = collect_arg_pairs(&args);

        assert!(pairs.contains(&("--write".into(), "/test/root".into())));
        assert!(pairs.contains(&("--exec".into(), "/test/root".into())));
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn test_sandbox_args_temp_dir_writable() {
        let server = BashServer::new_with_sandbox(
            vec![PathBuf::from("/test/root")],
            enabled_sandbox_config(),
        );
        let args = server.build_sandbox_args(
            Path::new("/test/root/workdir"),
            None,
            None,
            &[PathBuf::from("/test/root")],
        );
        let pairs = collect_arg_pairs(&args);

        assert!(pairs.contains(&("--write".into(), "/tmp".into())));
    }

    #[cfg(all(unix, target_os = "macos"))]
    #[test]
    fn test_sandbox_args_temp_dir_writable() {
        let server = BashServer::new_with_sandbox(
            vec![PathBuf::from("/test/root")],
            enabled_sandbox_config(),
        );
        let args = server.build_sandbox_args(
            Path::new("/test/root/workdir"),
            None,
            None,
            &[PathBuf::from("/test/root")],
        );
        let pairs = collect_arg_pairs(&args);

        assert!(pairs.contains(&("--write".into(), "/private/tmp".into())));
    }

    #[cfg(unix)]
    #[test]
    fn env_default_allowlist_vars_passed_through() {
        let _env_guard = env_lock();
        let _home = EnvVar::set("HOME", "/tmp/harnx-home-4-1");
        let _path = EnvVar::set("PATH", "/tmp/harnx-bin-4-1");
        let _secret = EnvVar::set("HARNX_TEST_SECRET_4_1", "hunter2");
        let _config_dir = EnvVar::unset("HARNX_CONFIG_DIR");

        let server = BashServer::new_with_sandbox(vec![], enabled_sandbox_config());
        let child_env = server.build_child_env();

        assert!(child_env
            .iter()
            .any(|(key, value)| key == "HOME" && value == "/tmp/harnx-home-4-1"));
        assert!(child_env
            .iter()
            .any(|(key, value)| key == "PATH" && value == "/tmp/harnx-bin-4-1"));
        assert!(
            !child_env
                .iter()
                .any(|(key, _)| key == "HARNX_TEST_SECRET_4_1"),
            "non-allowlisted env leaked into child env: {child_env:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_overrides_and_passthrough() {
        let _env_guard = env_lock();
        let _host_value = EnvVar::set("HARNX_TEST_CUSTOM_4_2", "from_host");
        let _config_dir = EnvVar::unset("HARNX_CONFIG_DIR");

        let mut passthrough_config = enabled_sandbox_config();
        passthrough_config.extra_env_passthrough = vec!["HARNX_TEST_CUSTOM_4_2".to_string()];
        let passthrough_server = BashServer::new_with_sandbox(vec![], passthrough_config);
        let passthrough_env = passthrough_server.build_child_env();
        assert!(passthrough_env
            .iter()
            .any(|(key, value)| { key == "HARNX_TEST_CUSTOM_4_2" && value == "from_host" }));

        let mut override_config = enabled_sandbox_config();
        override_config.extra_env_passthrough = vec!["HARNX_TEST_CUSTOM_4_2".to_string()];
        override_config.env_overrides = vec![(
            "HARNX_TEST_CUSTOM_4_2".to_string(),
            "overridden".to_string(),
        )];
        let override_server = BashServer::new_with_sandbox(vec![], override_config);
        let override_env = override_server.build_child_env();
        assert!(override_env
            .iter()
            .any(|(key, value)| { key == "HARNX_TEST_CUSTOM_4_2" && value == "overridden" }));
    }

    #[cfg(unix)]
    #[test]
    fn env_precedence_cli_over_passthrough_over_dotfile() {
        let _env_guard = env_lock();

        // Set host value for the var so passthrough can pick it up.
        let _host_value = EnvVar::set("HARNX_TEST_PRECEDENCE_VAR", "from_host_passthrough");

        // Point dotfile at a tempdir whose .env.bash sets a different value.
        let temp_dir = TestDir::new();
        std::fs::write(
            temp_dir.path().join(".env.bash"),
            "HARNX_TEST_PRECEDENCE_VAR=from_dotfile\n",
        )
        .unwrap();
        let _config_dir = EnvVar::set("HARNX_CONFIG_DIR", temp_dir.path().as_os_str());

        // Case 1: dotfile only (no passthrough, no override).
        // Expect dotfile value to win over (absent) default allowlist value.
        let dotfile_only = enabled_sandbox_config();
        let dotfile_server = BashServer::new_with_sandbox(vec![], dotfile_only);
        let dotfile_env = dotfile_server.build_child_env();
        assert!(dotfile_env
            .iter()
            .any(|(k, v)| { k == "HARNX_TEST_PRECEDENCE_VAR" && v == "from_dotfile" }));

        // Case 2: dotfile + passthrough → passthrough beats dotfile.
        let mut passthrough_cfg = enabled_sandbox_config();
        passthrough_cfg.extra_env_passthrough = vec!["HARNX_TEST_PRECEDENCE_VAR".to_string()];
        let passthrough_server = BashServer::new_with_sandbox(vec![], passthrough_cfg);
        let passthrough_env = passthrough_server.build_child_env();
        assert!(passthrough_env
            .iter()
            .any(|(k, v)| { k == "HARNX_TEST_PRECEDENCE_VAR" && v == "from_host_passthrough" }));

        // Case 3: dotfile + passthrough + override → override beats both.
        let mut override_cfg = enabled_sandbox_config();
        override_cfg.extra_env_passthrough = vec!["HARNX_TEST_PRECEDENCE_VAR".to_string()];
        override_cfg.env_overrides = vec![(
            "HARNX_TEST_PRECEDENCE_VAR".to_string(),
            "from_cli_override".to_string(),
        )];
        let override_server = BashServer::new_with_sandbox(vec![], override_cfg);
        let override_env = override_server.build_child_env();
        assert!(override_env
            .iter()
            .any(|(k, v)| { k == "HARNX_TEST_PRECEDENCE_VAR" && v == "from_cli_override" }));
    }

    #[cfg(unix)]
    #[test]
    fn env_bash_dotfile_loaded() {
        let _env_guard = env_lock();
        let temp_dir = TestDir::new();
        std::fs::write(
            temp_dir.path().join(".env.bash"),
            "# comment line\n\nHARNX_TEST_INJECT_4_3=s3cr3t\nHARNX_TEST_INJECT_KV_4_3=a=b\n",
        )
        .unwrap();
        let _config_dir = EnvVar::set("HARNX_CONFIG_DIR", temp_dir.path().as_os_str());

        let env_vars = load_bash_env_file();

        assert!(env_vars
            .iter()
            .any(|(key, value)| { key == "HARNX_TEST_INJECT_4_3" && value == "s3cr3t" }));
        assert!(env_vars
            .iter()
            .any(|(key, value)| { key == "HARNX_TEST_INJECT_KV_4_3" && value == "a=b" }));
        assert!(!env_vars.iter().any(|(key, _)| key == "# comment line"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_inputs_validation_rejects_paths_outside_roots() {
        let root = TestDir::new();
        let server =
            BashServer::new_with_sandbox(vec![root.path().to_path_buf()], enabled_sandbox_config());

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "cat /etc/passwd".to_string(),
                working_dir: Some(root.path().to_string_lossy().to_string()),
                timeout_secs: Some(15),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: Some(vec!["/etc".into()]),
                outputs: None,
            })
            .await;

        let err = result.unwrap_err();
        assert_eq!(err.code.0, -32602);
        assert!(
            err.message.contains("outside allowed roots")
                || err.message.contains("not under allowed roots")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_outputs_validation_rejects_paths_outside_roots() {
        let root = TestDir::new();
        let server =
            BashServer::new_with_sandbox(vec![root.path().to_path_buf()], enabled_sandbox_config());

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo hi".to_string(),
                working_dir: Some(root.path().to_string_lossy().to_string()),
                timeout_secs: Some(15),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: Some(vec!["/etc".into()]),
            })
            .await;

        let err = result.unwrap_err();
        assert_eq!(err.code.0, -32602);
        assert!(
            err.message.contains("outside allowed roots")
                || err.message.contains("not under allowed roots")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_inputs_relative_paths_resolved_against_working_dir() {
        // Verifies that a relative `inputs` path is resolved against the
        // tool's `working_dir` at validation time (not the server's CWD).
        // We don't run bash here — that's a separate concern covered by the
        // Linux-only sandbox-runtime tests below; we just check that
        // validation accepts the relative path.
        let root = TestDir::new();
        std::fs::create_dir(root.path().join("subdir")).unwrap();
        let roots = vec![root.path().to_path_buf()];

        let validated =
            parse_input_path_list(&Some(vec!["subdir".to_string()]), &roots, root.path())
                .expect("relative input path under working_dir must validate");

        let paths = validated.expect("Some(_) when input list provided");
        assert_eq!(paths.len(), 1);
        assert!(
            paths[0].ends_with("subdir"),
            "validated path should canonicalize to .../subdir, got {}",
            paths[0].display()
        );
        assert!(
            paths[0].starts_with(root.path().canonicalize().unwrap()),
            "validated path should be under root, got {}",
            paths[0].display()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_sandbox_exec_write_allowed() {
        if !sandbox_runtime_works() {
            return;
        }
        let root = TestDir::new();
        let server = sandboxed_server(vec![root.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo hi > out.txt".to_string(),
                working_dir: Some(root.path().to_string_lossy().to_string()),
                timeout_secs: Some(15),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        eprintln!(
            "sandbox write allowed output:
{text}"
        );
        assert_eq!(extract_field(&text, "exit_code"), "0");
        assert_eq!(
            std::fs::read_to_string(root.path().join("out.txt")).unwrap(),
            "hi
"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_sandbox_exec_write_denied_outside_root() {
        if !sandbox_runtime_works() {
            return;
        }
        let root = TestDir::new();
        let server = sandboxed_server(vec![root.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo hi > out.txt".to_string(),
                working_dir: Some(root.path().to_string_lossy().to_string()),
                timeout_secs: Some(15),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: Some(vec![]),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        eprintln!(
            "sandbox write denied output:
{text}"
        );
        let exit_code = extract_field(&text, "exit_code").parse::<i32>().unwrap();
        let denied = exit_code != 0
            || text.contains("denied")
            || text.contains("Permission")
            || text.contains("permission");
        assert!(
            denied,
            "expected sandbox denial evidence, got:
{text}"
        );
        assert!(!root.path().join("out.txt").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_sandbox_exec_custom_outputs() {
        if !sandbox_runtime_works() {
            return;
        }
        let root = TestDir::new();
        let other = TestDir::new();
        let server = sandboxed_server(vec![root.path().to_path_buf(), other.path().to_path_buf()]);
        let outputs = vec![other.path().to_string_lossy().to_string()];

        let fail_result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo hi > in_root.txt".to_string(),
                working_dir: Some(root.path().to_string_lossy().to_string()),
                timeout_secs: Some(15),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: Some(outputs.clone()),
            })
            .await
            .unwrap();
        let fail_text = text_content(&fail_result);
        eprintln!(
            "sandbox custom outputs fail output:
{fail_text}"
        );
        let fail_exit_code = extract_field(&fail_text, "exit_code")
            .parse::<i32>()
            .unwrap();
        assert_ne!(
            fail_exit_code, 0,
            "expected root write failure, got:
{fail_text}"
        );
        assert!(!root.path().join("in_root.txt").exists());

        let success_result = server
            .exec_command_impl(ExecCommandParams {
                command: format!("echo bye > {}", other.path().join("in_other.txt").display()),
                working_dir: Some(root.path().to_string_lossy().to_string()),
                timeout_secs: Some(15),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: Some(outputs),
            })
            .await
            .unwrap();
        let success_text = text_content(&success_result);
        eprintln!(
            "sandbox custom outputs success output:
{success_text}"
        );
        assert_eq!(extract_field(&success_text, "exit_code"), "0");
        assert_eq!(
            std::fs::read_to_string(other.path().join("in_other.txt")).unwrap(),
            "bye
"
        );
    }

    #[tokio::test]
    async fn test_working_dir_rejected_outside_roots() {
        let allowed = TestDir::new();
        let outside = TestDir::new();
        let server = BashServer::new(vec![allowed.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "pwd".to_string(),
                working_dir: Some(outside.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: None,
            })
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("outside allowed roots"));
    }

    #[tokio::test]
    async fn test_exec_rejects_empty_command() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "   ".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: None,
            })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_exec_nonzero_exit_code() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo boom >&2; exit 1".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(false));
        assert!(text.contains("exit_code: 1"));
    }

    #[tokio::test]
    async fn test_exec_basic_command() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo test".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(false));
        assert!(text.contains("exit_code: 0"));
        assert!(text.contains("test"));

        let stdout_log_path = PathBuf::from(extract_field(&text, "stdout_log_path"));
        let stderr_log_path = PathBuf::from(extract_field(&text, "stderr_log_path"));
        assert!(stdout_log_path.exists());
        assert!(stderr_log_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn env_secret_not_leaked_to_child() {
        let _env_guard = env_lock();
        let _secret = EnvVar::set("AWS_SECRET_ACCESS_KEY", "hunter2_4_4");
        let _config_dir = EnvVar::unset("HARNX_CONFIG_DIR");
        let root = TestDir::new();
        let server = BashServer::new_with_sandbox(
            vec![root.path().to_path_buf()],
            disabled_sandbox_config(),
        );

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo ${AWS_SECRET_ACCESS_KEY:-empty}".to_string(),
                working_dir: Some(root.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(false));
        assert!(text.contains("exit_code: 0"));
        assert!(text.contains("empty"), "unexpected exec output: {text}");
        assert!(
            !text.contains("hunter2_4_4"),
            "secret leaked into child output: {text}"
        );
    }

    #[tokio::test]
    async fn test_exec_timeout() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "sleep 10".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(1),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(true));
        assert!(text.contains("command timed out after 1s and was terminated"));
        assert!(text.contains("stdout_log_path:"));
        assert!(text.contains("stderr_log_path:"));
    }

    #[tokio::test]
    async fn test_exec_truncation_mentions_log_paths_and_read_exec_log_works() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "printf 'out1\nout2\nout3\n'; printf 'err1\nerr2\n' >&2".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: Some(1),
                tail_lines: Some(1),
                max_output_bytes: Some(16),
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");
        assert!(text.contains("full log via read_exec_log"));
        assert!(text.contains(&execution_id));

        let stdout_read = server
            .read_exec_log_impl(ReadExecLogParams {
                execution_id: execution_id.clone(),
                stream: "stdout".to_string(),
                offset: None,
                limit: None,
                tail: None,
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();
        let stdout_text = text_content(&stdout_read);
        assert!(stdout_text.contains("1: out1"));
        assert!(stdout_text.contains("2: out2"));
        assert!(stdout_text.contains("3: out3"));

        let stderr_read = server
            .read_exec_log_impl(ReadExecLogParams {
                execution_id,
                stream: "stderr".to_string(),
                offset: None,
                limit: None,
                tail: Some(1),
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();
        let stderr_text = text_content(&stderr_read);
        assert!(stderr_text.contains("2: err2"));
        assert!(stderr_text.contains("showing last 1 of 2 matching lines"));
    }

    #[tokio::test]
    async fn test_read_exec_log_rejects_invalid_stream() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .read_exec_log_impl(ReadExecLogParams {
                execution_id: "exec-test".to_string(),
                stream: "invalid".to_string(),
                offset: None,
                limit: None,
                tail: None,
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("stream must be 'stdout' or 'stderr'"));
    }

    #[tokio::test]
    async fn test_cleanup_log_dir_removes_temp_logs() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo cleanup".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let stdout_log_path = PathBuf::from(extract_field(&text, "stdout_log_path"));
        let log_dir = stdout_log_path.parent().unwrap().to_path_buf();
        assert!(log_dir.exists());

        server.cleanup_log_dir().unwrap();
        assert!(!log_dir.exists());
    }

    #[tokio::test]
    async fn test_spawn_and_wait() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "echo background && sleep 1".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .wait_impl(WaitParams {
                execution_id,
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: Some(10),
                max_output_bytes: None,
                grep: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("status: exited"));
        assert!(text.contains("background"));
    }

    #[tokio::test]
    async fn test_spawn_wait_timeout() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "sleep 5".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .wait_impl(WaitParams {
                execution_id,
                timeout_secs: Some(1),
                head_lines: None,
                tail_lines: Some(10),
                max_output_bytes: None,
                grep: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("status: running"));
    }

    #[tokio::test]
    async fn test_spawn_and_terminate() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "sleep 30".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .terminate_impl(TerminateParams {
                execution_id,
                signal: Some("SIGTERM".to_string()),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("signal: SIGTERM"));
    }

    #[tokio::test]
    async fn test_wait_unknown_execution_id() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .wait_impl(WaitParams {
                execution_id: "exec-does-not-exist".to_string(),
                timeout_secs: Some(1),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                grep: None,
            })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_terminate_unknown_execution_id() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .terminate_impl(TerminateParams {
                execution_id: "exec-does-not-exist".to_string(),
                signal: None,
            })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_spawn_with_output() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "for i in 1 2 3; do echo line$i; done".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                inputs: None,
                outputs: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .wait_impl(WaitParams {
                execution_id,
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: Some(10),
                max_output_bytes: None,
                grep: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("exit_code: 0"));
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("line3"));
    }

    #[tokio::test]
    async fn test_exec_with_outputs_uses_targeted_snapshot() {
        let temp_dir = TestDir::new();
        init_git_repo(temp_dir.path()).await;
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "printf 'one\n' > specific_file.txt && printf 'two\n' > other_file.txt"
                    .to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: Some(20),
                max_output_bytes: None,
                inputs: None,
                outputs: Some(vec!["specific_file.txt".to_string()]),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("exit_code: 0"));
        assert!(text.contains("specific_file.txt"));
        assert!(!text.contains("other_file.txt"));
    }

    #[tokio::test]
    async fn test_exec_with_empty_outputs_skips_snapshot() {
        let temp_dir = TestDir::new();
        init_git_repo(temp_dir.path()).await;
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "printf 'one\n' > specific_file.txt && printf 'two\n' > other_file.txt"
                    .to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: Some(20),
                max_output_bytes: None,
                inputs: None,
                outputs: Some(vec![]),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("exit_code: 0"));
        assert!(!text.contains("diff --git"));
        assert!(!text.contains("specific_file.txt"));
        assert!(!text.contains("other_file.txt"));
    }

    #[tokio::test]
    async fn test_spawn_wait_with_outputs_uses_targeted_snapshot() {
        let temp_dir = TestDir::new();
        init_git_repo(temp_dir.path()).await;
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "printf 'one\n' > out.txt && printf 'two\n' > other.txt".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                inputs: None,
                outputs: Some(vec!["out.txt".to_string()]),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .wait_impl(WaitParams {
                execution_id,
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: Some(20),
                max_output_bytes: None,
                grep: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let diff_text = result
            .content
            .iter()
            .skip(2)
            .filter_map(|content| match &content.raw {
                rmcp::model::RawContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("exit_code: 0"));
        assert!(diff_text.contains("out.txt"));
        assert!(!diff_text.contains("other.txt"));
    }
}
