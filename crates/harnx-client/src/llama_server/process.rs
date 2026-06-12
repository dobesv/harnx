#[cfg(unix)]
use anyhow::{anyhow, bail, Context, Result};
#[cfg(unix)]
use bytes::Bytes;
#[cfg(unix)]
use harnx_core::config_paths::data_dir;
#[cfg(unix)]
use http_body_util::{BodyExt, Empty};
#[cfg(unix)]
use hyper::{Method, Request, StatusCode, Uri};
#[cfg(unix)]
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
#[cfg(unix)]
use hyperlocal::{UnixConnector, Uri as UnixUri};
#[cfg(unix)]
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::{
    collections::{HashMap, VecDeque},
    env,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};
#[cfg(unix)]
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
    time::{sleep, Instant},
};

#[cfg(unix)]
type HealthClient = Client<UnixConnector, Empty<Bytes>>;

#[cfg(unix)]
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(200);
#[cfg(unix)]
const STDERR_TAIL_LIMIT: usize = 64;
#[cfg(unix)]
const SOCKET_HASH_LEN: usize = 12;
#[cfg(unix)]
const LLAMA_SERVER_BIN_ENV: &str = "HARNX_LLAMA_SERVER_BIN";

/// Global registry of process managers, keyed by full process identity.
/// Ensures one manager per unique llama-server subprocess config.
#[cfg(unix)]
static MANAGERS: OnceLock<std::sync::Mutex<HashMap<ProcessIdentity, Arc<LlamaServerProcessManager>>>> =
    OnceLock::new();

/// Get or create process manager for given config.
///
/// Registry key covers all process-affecting fields so identical configs share
/// one manager while distinct subprocess configs remain isolated.
#[cfg(unix)]
pub fn get_or_create_manager(config: &LlamaServerProcessConfig) -> Result<Arc<LlamaServerProcessManager>> {
    let identity = ProcessIdentity::from_config(config)?;
    let managers = MANAGERS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));

    if let Some(existing) = managers.lock().unwrap().get(&identity).cloned() {
        return Ok(existing);
    }

    let manager = Arc::new(LlamaServerProcessManager::new(config.clone())?);
    let mut managers_guard = managers.lock().unwrap();
    Ok(managers_guard.entry(identity).or_insert_with(|| manager.clone()).clone())
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProcessIdentity {
    canonical: String,
}

#[cfg(unix)]
impl ProcessIdentity {
    fn from_config(config: &LlamaServerProcessConfig) -> Result<Self> {
        let resolved_binary_path = discover_binary_path_sync(config.binary_path.as_deref())?;
        let resolved_socket_path = resolve_socket_path_for_config(config)?;

        let canonical = [
            format!("model_path={}", config.model_path.display()),
            format!("binary_path={}", resolved_binary_path.display()),
            format!("socket_path={}", resolved_socket_path.display()),
            format!(
                "context_size={}",
                config
                    .context_size
                    .map_or_else(|| "none".to_string(), |value| value.to_string())
            ),
            format!(
                "gpu_layers={}",
                config
                    .gpu_layers
                    .map_or_else(|| "none".to_string(), |value| value.to_string())
            ),
            format!(
                "threads={}",
                config
                    .threads
                    .map_or_else(|| "none".to_string(), |value| value.to_string())
            ),
            format!("extra_args={}", config.extra_args.join("\u{1f}")),
        ]
        .join("\n");

        Ok(Self { canonical })
    }

    fn short_hash(&self) -> String {
        short_hash(&self.canonical)
    }
}

#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct LlamaServerProcessConfig {
    pub model_path: PathBuf,
    pub binary_path: Option<PathBuf>,
    pub socket_path: Option<PathBuf>,
    pub context_size: Option<u32>,
    pub gpu_layers: Option<i32>,
    pub threads: Option<u32>,
    pub extra_args: Vec<String>,
    pub ready_timeout: Duration,
}

#[cfg(unix)]
impl LlamaServerProcessConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            binary_path: None,
            socket_path: None,
            context_size: None,
            gpu_layers: None,
            threads: None,
            extra_args: Vec::new(),
            ready_timeout: Duration::from_secs(5 * 60),
        }
    }
}

#[cfg(unix)]
impl Default for LlamaServerProcessConfig {
    fn default() -> Self {
        Self::new("")
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct LlamaServerProcessManager {
    config: LlamaServerProcessConfig,
    state: Mutex<Option<Arc<RunningServer>>>,
    spawn_count: Arc<AtomicUsize>,
}

#[cfg(unix)]
impl LlamaServerProcessManager {
    pub fn new(config: LlamaServerProcessConfig) -> Result<Self> {
        if cfg!(windows) {
            bail!("llama-server provider requires a Unix platform (uses unix domain sockets)");
        }
        if config.model_path.as_os_str().is_empty() {
            bail!("llama-server model_path is required");
        }
        Ok(Self {
            config,
            state: Mutex::new(None),
            spawn_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub async fn socket_path(&self) -> Result<PathBuf> {
        Ok(self.ensure_ready().await?.socket_path.clone())
    }

    pub async fn ensure_ready(&self) -> Result<Arc<RunningServer>> {
        let mut state = self.state.lock().await;

        if let Some(running) = state.as_ref() {
            if running.child_try_wait().await?.is_none() {
                return Ok(running.clone());
            }
            warn!(
                "llama-server process for {} exited after startup; respawning",
                self.config.model_path.display()
            );
            state.take();
        }

        let mut running = spawn_server(self.config.clone()).await?;
        wait_until_ready(&mut running, self.config.ready_timeout).await?;
        self.spawn_count.fetch_add(1, Ordering::SeqCst);
        let running = Arc::new(running);
        *state = Some(running.clone());
        Ok(running)
    }

    pub fn spawn_count(&self) -> usize {
        self.spawn_count.load(Ordering::SeqCst)
    }
}

#[cfg(unix)]
impl Drop for LlamaServerProcessManager {
    fn drop(&mut self) {
        if let Some(running) = self.state.get_mut().take() {
            let _ = cleanup_socket_path(&running.socket_path);
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct RunningServer {
    socket_path: PathBuf,
    child: Arc<Mutex<Child>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

#[cfg(unix)]
impl RunningServer {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().await.iter().cloned().collect()
    }

    async fn child_try_wait(&self) -> Result<Option<std::process::ExitStatus>> {
        let mut child = self.child.lock().await;
        child
            .try_wait()
            .context("failed to query llama-server process state")
    }
}

#[cfg(unix)]
impl Drop for RunningServer {
    fn drop(&mut self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        let _ = cleanup_socket_path(&self.socket_path);
    }
}

#[cfg(unix)]
pub async fn discover_binary_path(configured_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured_path {
        return validate_binary_path(path, "binary_path config override");
    }

    if let Some(env_path) = env::var_os(LLAMA_SERVER_BIN_ENV) {
        return validate_binary_path(Path::new(&env_path), LLAMA_SERVER_BIN_ENV);
    }

    which::which("llama-server").with_context(|| {
        "Unable to find `llama-server` in binary_path config, HARNX_LLAMA_SERVER_BIN, or PATH. Install llama.cpp from GitHub releases or `brew install llama.cpp`."
    })
}

#[cfg(unix)]
fn discover_binary_path_sync(configured_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured_path {
        return validate_binary_path(path, "binary_path config override");
    }

    if let Some(env_path) = env::var_os(LLAMA_SERVER_BIN_ENV) {
        return validate_binary_path(Path::new(&env_path), LLAMA_SERVER_BIN_ENV);
    }

    which::which("llama-server").with_context(|| {
        "Unable to find `llama-server` in binary_path config, HARNX_LLAMA_SERVER_BIN, or PATH. Install llama.cpp from GitHub releases or `brew install llama.cpp`."
    })
}

#[cfg(unix)]
fn default_socket_path(parent_pid: u32, identity: &ProcessIdentity) -> PathBuf {
    data_dir().join(format!(
        "llama-server-{parent_pid}-{}.sock",
        identity.short_hash()
    ))
}

#[cfg(unix)]
fn resolve_socket_path_for_config(config: &LlamaServerProcessConfig) -> Result<PathBuf> {
    if let Some(path) = config.socket_path.as_deref() {
        return Ok(path.to_path_buf());
    }

    let model_canonical = format!("model_path={}", config.model_path.display());
    let model_identity = ProcessIdentity {
        canonical: model_canonical,
    };
    Ok(default_socket_path(std::process::id(), &model_identity))
}

#[cfg(unix)]
fn resolve_socket_path(configured_path: Option<&Path>, identity: &ProcessIdentity) -> PathBuf {
    configured_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_socket_path(std::process::id(), identity))
}

#[cfg(unix)]
async fn spawn_server(config: LlamaServerProcessConfig) -> Result<RunningServer> {
    let binary_path = discover_binary_path(config.binary_path.as_deref()).await?;
    let identity = ProcessIdentity::from_config(&config)?;
    let socket_path = resolve_socket_path(config.socket_path.as_deref(), &identity);
    prepare_socket_path(&socket_path).await?;

    let mut command = Command::new(&binary_path);
    command
        .arg("-m")
        .arg(&config.model_path)
        .arg("--host")
        .arg(&socket_path);

    if let Some(context_size) = config.context_size {
        command.arg("-c").arg(context_size.to_string());
    }
    if let Some(gpu_layers) = config.gpu_layers {
        command.arg("-ngl").arg(gpu_layers.to_string());
    }
    if let Some(threads) = config.threads {
        command.arg("-t").arg(threads.to_string());
    }
    command.args(&config.extra_args);
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    debug!("spawning llama-server: {:?}", command);
    let mut child = command.spawn().with_context(|| {
        format!(
            "Failed to spawn llama-server binary `{}`",
            binary_path.display()
        )
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture llama-server stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture llama-server stderr"))?;

    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LIMIT)));
    let stdout_task = tokio::spawn(log_output("llama-server stdout", stdout, None));
    let stderr_task = tokio::spawn(log_output(
        "llama-server stderr",
        stderr,
        Some(stderr_tail.clone()),
    ));

    Ok(RunningServer {
        socket_path,
        child: Arc::new(Mutex::new(child)),
        stderr_tail,
        stdout_task,
        stderr_task,
    })
}

#[cfg(unix)]
async fn prepare_socket_path(socket_path: &Path) -> Result<()> {
    let parent = socket_path.parent().ok_or_else(|| {
        anyhow!(
            "Invalid llama-server socket path `{}`: missing parent directory",
            socket_path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .await
        .with_context(|| format!("Failed to create socket directory `{}`", parent.display()))?;

    cleanup_socket_path(socket_path)?;
    Ok(())
}

#[cfg(unix)]
fn cleanup_socket_path(socket_path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => {
            if metadata.file_type().is_socket() {
                std::fs::remove_file(socket_path).with_context(|| {
                    format!("Failed to remove stale socket `{}`", socket_path.display())
                })?;
            } else {
                bail!(
                    "refusing to remove non-socket file at {}",
                    socket_path.display()
                );
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to inspect socket path `{}`", socket_path.display())
            });
        }
    }

    Ok(())
}

#[cfg(unix)]
async fn wait_until_ready(running: &mut RunningServer, timeout: Duration) -> Result<()> {
    let client: HealthClient = Client::builder(TokioExecutor::new()).build(UnixConnector);
    let started = Instant::now();

    loop {
        if let Some(status) = running.child_try_wait().await? {
            let tail = format_stderr_tail(&running.stderr_tail().await);
            bail!("llama-server exited before becoming ready (status: {status}).{tail}");
        }

        match check_health(&client, &running.socket_path).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => {
                debug!(
                    "llama-server health check pending on {}: {err}",
                    running.socket_path.display()
                );
            }
        }

        if started.elapsed() >= timeout {
            let tail = format_stderr_tail(&running.stderr_tail().await);
            bail!(
                "Timed out waiting {:?} for llama-server readiness on {}.{tail}",
                timeout,
                running.socket_path.display()
            );
        }

        sleep(HEALTH_POLL_INTERVAL).await;
    }
}

#[cfg(unix)]
async fn check_health(client: &HealthClient, socket_path: &Path) -> Result<bool> {
    let uri: Uri = UnixUri::new(socket_path, "/health").into();
    let response = client
        .request(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Empty::new())?,
        )
        .await;

    let mut response = match response {
        Ok(response) => response,
        Err(err) => return Err(err).context("health check transport error"),
    };

    if response.status() != StatusCode::OK {
        while let Some(frame) = response.body_mut().frame().await {
            let _ = frame?;
        }
        return Ok(false);
    }

    let mut bytes = Vec::new();
    while let Some(frame) = response.body_mut().frame().await {
        let frame = frame?;
        if let Some(chunk) = frame.data_ref() {
            bytes.extend_from_slice(chunk);
        }
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "Invalid llama-server /health response: {}",
            String::from_utf8_lossy(&bytes)
        )
    })?;
    Ok(body
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| status == "ok"))
}

#[cfg(unix)]
async fn log_output<R>(
    label: &'static str,
    reader: R,
    stderr_tail: Option<Arc<Mutex<VecDeque<String>>>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        debug!("{}: {}", label, line);
        if let Some(stderr_tail) = &stderr_tail {
            let mut tail = stderr_tail.lock().await;
            if tail.len() == STDERR_TAIL_LIMIT {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
}

#[cfg(unix)]
fn validate_binary_path(path: &Path, source: &str) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("Configured llama-server path from {source} is empty");
    }
    if path.components().count() == 1 {
        return which::which(path).with_context(|| {
            format!(
                "Configured llama-server path `{}` from {source} was not found in PATH",
                path.display()
            )
        });
    }

    let metadata = std::fs::metadata(path).with_context(|| {
        format!(
            "Configured llama-server path `{}` from {source} does not exist",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        bail!(
            "Configured llama-server path `{}` from {source} is not a file",
            path.display()
        );
    }
    Ok(path.to_path_buf())
}

#[cfg(unix)]
fn format_stderr_tail(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("\nRecent stderr:\n{}", lines.join("\n"))
    }
}

#[cfg(unix)]
fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(SOCKET_HASH_LEN);
    for byte in digest.iter().take(SOCKET_HASH_LEN / 2) {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{ffi::OsString, future::Future, sync::Arc};
    use tempfile::TempDir;
    use tokio::net::UnixListener;

    fn test_config(model_path: &str) -> LlamaServerProcessConfig {
        LlamaServerProcessConfig {
            model_path: PathBuf::from(model_path),
            binary_path: Some(PathBuf::from("/bin/echo")),
            socket_path: None,
            context_size: Some(512),
            gpu_layers: Some(0),
            threads: Some(2),
            extra_args: vec!["--alias".into(), "test".into()],
            ready_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn default_socket_path_uses_harnx_data_dir_and_hash() {
        let identity = ProcessIdentity::from_config(&test_config("/models/one.gguf")).unwrap();
        let path = default_socket_path(4242, &identity);
        assert!(path.ends_with(format!("harnx/llama-server-4242-{}.sock", identity.short_hash())));
    }

    #[test]
    fn default_socket_path_differs_for_different_models() {
        let one = ProcessIdentity::from_config(&test_config("/models/one.gguf")).unwrap();
        let two = ProcessIdentity::from_config(&test_config("/models/two.gguf")).unwrap();
        assert_ne!(default_socket_path(4242, &one), default_socket_path(4242, &two));
    }

    #[test]
    fn resolve_socket_path_prefers_override() {
        let custom = PathBuf::from("/tmp/custom-llama.sock");
        let identity = ProcessIdentity::from_config(&test_config("/models/one.gguf")).unwrap();
        assert_eq!(resolve_socket_path(Some(&custom), &identity), custom);
    }

    #[test]
    fn process_identity_matches_identical_configs() {
        let a = ProcessIdentity::from_config(&test_config("/models/shared.gguf")).unwrap();
        let b = ProcessIdentity::from_config(&test_config("/models/shared.gguf")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn process_identity_changes_with_process_affecting_fields() {
        let base = test_config("/models/shared.gguf");
        let mut variants = Vec::new();

        let mut different_socket = base.clone();
        different_socket.socket_path = Some(PathBuf::from("/tmp/alt.sock"));
        variants.push(different_socket);

        let mut different_ctx = base.clone();
        different_ctx.context_size = Some(1024);
        variants.push(different_ctx);

        let mut different_gpu = base.clone();
        different_gpu.gpu_layers = Some(4);
        variants.push(different_gpu);

        let mut different_threads = base.clone();
        different_threads.threads = Some(8);
        variants.push(different_threads);

        let mut different_args = base.clone();
        different_args.extra_args.push("--flash-attn".into());
        variants.push(different_args);

        let mut different_binary = base.clone();
        different_binary.binary_path = Some(PathBuf::from("/bin/cat"));
        variants.push(different_binary);

        let base_identity = ProcessIdentity::from_config(&base).unwrap();
        for variant in variants {
            let identity = ProcessIdentity::from_config(&variant).unwrap();
            assert_ne!(base_identity, identity);
        }
    }

    #[tokio::test]
    async fn discover_binary_path_reports_install_hint() {
        let temp = TempDir::new().unwrap();
        let old_path = env::var_os("PATH");
        let old_env = env::var_os(LLAMA_SERVER_BIN_ENV);
        env::set_var("PATH", temp.path());
        env::remove_var(LLAMA_SERVER_BIN_ENV);

        let err = discover_binary_path(None).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Unable to find `llama-server`"));
        assert!(message.contains("brew install llama.cpp"));

        restore_env("PATH", old_path);
        restore_env(LLAMA_SERVER_BIN_ENV, old_env);
    }

    #[test]
    fn cleanup_socket_path_rejects_non_socket_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("not-a-socket.sock");
        std::fs::write(&path, b"regular file").unwrap();

        let err = cleanup_socket_path(&path).unwrap_err();
        assert!(err.to_string().contains("refusing to remove non-socket file"));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn concurrent_ensure_ready_spawns_once_for_live_state() {
        let child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let fake = Arc::new(RunningServer {
            socket_path: PathBuf::from("/tmp/live.sock"),
            child: Arc::new(Mutex::new(child)),
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            stdout_task: tokio::spawn(async {}),
            stderr_task: tokio::spawn(async {}),
        });
        let manager = LlamaServerProcessManager {
            config: test_config("/models/live.gguf"),
            state: Mutex::new(Some(fake.clone())),
            spawn_count: Arc::new(AtomicUsize::new(0)),
        };

        let (a, b) = tokio::join!(manager.ensure_ready(), manager.ensure_ready());
        let a = a.unwrap();
        let b = b.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(manager.spawn_count(), 0);
    }

    #[test]
    fn get_or_create_manager_returns_err_for_invalid_config() {
        let temp = TempDir::new().unwrap();
        let old_path = env::var_os("PATH");
        let old_env = env::var_os(LLAMA_SERVER_BIN_ENV);
        env::set_var("PATH", temp.path());
        env::remove_var(LLAMA_SERVER_BIN_ENV);

        let err = get_or_create_manager(&LlamaServerProcessConfig::default()).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("llama-server model_path is required")
                || message.contains("Unable to find `llama-server`")
        );

        restore_env("PATH", old_path);
        restore_env(LLAMA_SERVER_BIN_ENV, old_env);
    }

    #[tokio::test]
    #[ignore = "requires real llama-server binary and GGUF model via HARNX_LLAMA_SERVER_TEST_* env vars"]
    async fn real_llama_server_smoke_test() -> Result<()> {
        let binary_path = env::var_os("HARNX_LLAMA_SERVER_TEST_BIN")
            .map(PathBuf::from)
            .context("HARNX_LLAMA_SERVER_TEST_BIN required")?;
        let model_path = env::var_os("HARNX_LLAMA_SERVER_TEST_MODEL")
            .map(PathBuf::from)
            .context("HARNX_LLAMA_SERVER_TEST_MODEL required")?;
        let socket_path =
            env::temp_dir().join(format!("harnx-llama-test-{}.sock", std::process::id()));

        let manager = LlamaServerProcessManager::new(LlamaServerProcessConfig {
            model_path,
            binary_path: Some(binary_path),
            socket_path: Some(socket_path.clone()),
            context_size: Some(512),
            gpu_layers: Some(0),
            threads: Some(2),
            extra_args: Vec::new(),
            ready_timeout: Duration::from_secs(5 * 60),
        })?;

        let running = manager.ensure_ready().await?;
        assert_eq!(running.socket_path(), socket_path.as_path());
        assert!(socket_path.exists());
        drop(manager);
        sleep(Duration::from_secs(1)).await;
        assert!(!socket_path.exists());
        Ok(())
    }

    fn restore_env(key: &str, value: Option<OsString>) {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    #[allow(dead_code)]
    async fn _unix_listener_ready(socket_path: &Path) -> Result<()> {
        if socket_path.exists() {
            fs::remove_file(socket_path).await?;
        }
        let _listener = UnixListener::bind(socket_path)?;
        Ok(())
    }

    #[allow(dead_code)]
    async fn _join_ok<F>(future: F)
    where
        F: Future<Output = Result<()>>,
    {
        future.await.unwrap();
    }
}

#[cfg(windows)]
use anyhow::{bail, Result};
#[cfg(windows)]
use std::{path::PathBuf, time::Duration};

#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct LlamaServerProcessConfig {
    pub model_path: PathBuf,
    pub binary_path: Option<PathBuf>,
    pub socket_path: Option<PathBuf>,
    pub context_size: Option<u32>,
    pub gpu_layers: Option<i32>,
    pub threads: Option<u32>,
    pub extra_args: Vec<String>,
    pub ready_timeout: Duration,
}

#[cfg(windows)]
#[derive(Debug)]
pub struct LlamaServerProcessManager;

#[cfg(windows)]
impl LlamaServerProcessManager {
    pub fn new(_config: LlamaServerProcessConfig) -> Result<Self> {
        bail!("llama-server provider requires a Unix platform (uses unix domain sockets)")
    }
}
